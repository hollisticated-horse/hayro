// Copyright 2025 the Vello Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

use crate::coarse::{Wide, WideTile};
use crate::encode::{EncodeExt, EncodedPaint};
use crate::fine::{COLOR_COMPONENTS, Fine, to_rgba8};
use crate::flatten::Line;
use crate::mask::Mask;
use crate::paint::{Paint, PaintType};
use crate::pixmap::Pixmap;
use crate::strip::Strip;
use crate::tile::{Tile, Tiles};
use crate::{flatten, strip};
use hayro_interpret::FillRule;
use kurbo::{Affine, BezPath, Cap, Join, Rect, Shape, Stroke};
use log::warn;
use rayon::ThreadPoolBuilder;
use rayon::join;
use std::cell::RefCell;
use std::sync::OnceLock;
use std::vec;
use std::vec::Vec;

thread_local! {
    static FINE_POOL: RefCell<Vec<Fine>> = RefCell::new(Vec::new());
}

pub(crate) const DEFAULT_TOLERANCE: f64 = 0.1;
/// A render context.
#[derive(Debug)]
pub(crate) struct RenderContext {
    pub(crate) width: u16,
    pub(crate) height: u16,
    pub(crate) wide: Wide,
    pub(crate) alphas: Vec<u8>,
    pub(crate) line_buf: Vec<Line>,
    pub(crate) tiles: Tiles,
    pub(crate) strip_buf: Vec<Strip>,
    paint_clip_path: Option<BezPath>,
    pub(crate) stroke: Stroke,
    pub(crate) transform: Affine,
    pub(crate) fill_rule: FillRule,
    pub(crate) encoded_paints: Vec<EncodedPaint>,
    pub(crate) anti_aliasing: bool,
    thread_limit: Option<usize>,
}

impl RenderContext {
    /// Create a new render context with the given width and height in pixels.
    pub(crate) fn new(width: u16, height: u16, thread_limit: Option<usize>) -> Self {
        let wide = Wide::new(width, height);

        let alphas = vec![];
        let line_buf = vec![];
        let tiles = Tiles::new();
        let strip_buf = vec![];

        let transform = Affine::IDENTITY;
        let fill_rule = FillRule::NonZero;
        let stroke = Stroke {
            width: 1.0,
            join: Join::Bevel,
            start_cap: Cap::Butt,
            end_cap: Cap::Butt,
            ..Default::default()
        };
        let encoded_paints = vec![];
        let anti_aliasing = true;
        let thread_limit = thread_limit.filter(|&n| n > 0);

        Self {
            width,
            height,
            wide,
            alphas,
            line_buf,
            tiles,
            strip_buf,
            transform,
            paint_clip_path: None,
            fill_rule,
            stroke,
            encoded_paints,
            anti_aliasing,
            thread_limit,
        }
    }

    pub(crate) fn thread_limit(&self) -> Option<usize> {
        self.thread_limit
    }

    fn encode_paint(&mut self, paint_type: PaintType) -> Paint {
        match paint_type {
            PaintType::Solid(s) => {
                self.paint_clip_path = None;
                s.into()
            }
            PaintType::Image(i) => {
                self.paint_clip_path = None;
                i.encode_into(&mut self.encoded_paints)
            }
            PaintType::ShadingPattern(s) => {
                self.paint_clip_path = s.shading.clip_path.clone();
                s.encode_into(&mut self.encoded_paints)
            }
        }
    }

    /// Fill a path.
    pub(crate) fn fill_path(&mut self, path: &BezPath, paint_type: PaintType, mask: Option<Mask>) {
        let paint = self.encode_paint(paint_type);
        self.apply_paint_bbox();
        flatten::fill(path, self.transform, &mut self.line_buf);
        self.render_path(self.fill_rule, paint, mask);
        self.unapply_paint_bbox();
    }

    fn apply_paint_bbox(&mut self) {
        if let Some(clip_path) = self.paint_clip_path.clone() {
            self.push_layer(Some(&clip_path), None, None);
        }
    }

    fn unapply_paint_bbox(&mut self) {
        if self.paint_clip_path.is_some() {
            self.pop_layer();
        }
    }

    /// Stroke a path.
    pub(crate) fn stroke_path(
        &mut self,
        path: &BezPath,
        paint_type: PaintType,
        mask: Option<Mask>,
    ) {
        let paint = self.encode_paint(paint_type);
        self.apply_paint_bbox();
        flatten::stroke(path, &self.stroke, self.transform, &mut self.line_buf);
        self.render_path(FillRule::NonZero, paint, mask);
        self.unapply_paint_bbox();
    }

    /// Fill a rectangle.
    pub(crate) fn fill_rect(&mut self, rect: &Rect, paint_type: PaintType, mask: Option<Mask>) {
        self.fill_path(&rect.to_path(DEFAULT_TOLERANCE), paint_type, mask);
    }

    /// Push a new layer with the given properties.
    ///
    /// Note that the mask, if provided, needs to have the same size as the render context. Otherwise,
    /// it will be ignored. In addition to that, the mask will not be affected by the current
    /// transformation matrix in place.
    pub(crate) fn push_layer(
        &mut self,
        clip_path: Option<&BezPath>,
        opacity: Option<f32>,
        mask: Option<Mask>,
    ) {
        let clip = if let Some(c) = clip_path {
            flatten::fill(c, Affine::IDENTITY, &mut self.line_buf);
            self.make_strips(self.fill_rule);
            Some((self.strip_buf.as_slice(), self.fill_rule))
        } else {
            None
        };

        let mask = mask.and_then(|m| {
            if m.width() != self.width || m.height() != self.height {
                None
            } else {
                Some(m)
            }
        });

        self.wide.push_layer(clip, mask, opacity.unwrap_or(1.0));
    }

    /// Pop the last-pushed layer.
    pub(crate) fn pop_layer(&mut self) {
        self.wide.pop_layer();
    }

    /// Set the current stroke.
    pub(crate) fn set_stroke(&mut self, stroke: Stroke) {
        self.stroke = stroke;
    }

    /// Set the current fill rule.
    pub(crate) fn set_fill_rule(&mut self, fill_rule: FillRule) {
        self.fill_rule = fill_rule;
    }

    /// Set the current transform.
    pub(crate) fn set_transform(&mut self, transform: Affine) {
        self.transform = transform;
    }

    /// Render the current context into a buffer.
    /// The buffer is expected to be in premultiplied RGBA8 format with length `width * height * 4`
    pub(crate) fn render_to_buffer(&self, buffer: &mut [u8], width: u16, height: u16) {
        assert!(
            !self.wide.has_layers(),
            "some layers haven't been popped yet"
        );
        assert_eq!(
            buffer.len(),
            (width as usize) * (height as usize) * 4,
            "provided width ({}) and height ({}) do not match buffer size ({})",
            width,
            height,
            buffer.len(),
        );

        let width_tiles = usize::from(self.wide.width_tiles());
        let height_tiles = usize::from(self.wide.height_tiles());
        if width_tiles == 0 || height_tiles == 0 {
            return;
        }

        let effective_threads = self.thread_limit.or_else(|| configured_threads());

        match effective_threads {
            Some(threads) if threads <= 1 => {
                render_tiles_sequential(self, width, height, width_tiles, height_tiles, buffer)
            }
            maybe_threads => {
                if let Some(threads) = maybe_threads {
                    install_global_pool(threads);
                }
                render_tiles_parallel(self, width, height, width_tiles, height_tiles, buffer);
            }
        }
    }

    /// Render the current context into a pixmap.
    pub(crate) fn render_to_pixmap(&self, pixmap: &mut Pixmap) {
        let width = pixmap.width();
        let height = pixmap.height();
        self.render_to_buffer(pixmap.data_as_u8_slice_mut(), width, height);
    }

    pub(crate) fn set_anti_aliasing(&mut self, val: bool) {
        self.anti_aliasing = val;
    }

    // Assumes that `line_buf` contains the flattened path.
    fn render_path(&mut self, fill_rule: FillRule, paint: Paint, mask: Option<Mask>) {
        self.make_strips(fill_rule);
        self.wide.generate(&self.strip_buf, fill_rule, paint, mask);
    }

    fn make_strips(&mut self, fill_rule: FillRule) {
        self.tiles
            .make_tiles(&self.line_buf, self.width, self.height);
        self.tiles.sort_tiles();

        strip::render(
            &self.tiles,
            &mut self.strip_buf,
            &mut self.alphas,
            fill_rule,
            &self.line_buf,
            self.anti_aliasing,
        );
    }
}

fn render_tiles_sequential(
    ctx: &RenderContext,
    width: u16,
    height: u16,
    width_tiles: usize,
    height_tiles: usize,
    buffer: &mut [u8],
) {
    let mut fine = Fine::new(width, height);
    let width_usize = width as usize;
    let height_usize = height as usize;
    let max_tile_width = usize::from(WideTile::WIDTH);
    let max_tile_height = usize::from(Tile::HEIGHT);

    for y_tile in 0..height_tiles as u16 {
        for x_tile in 0..width_tiles as u16 {
            let wtile = ctx.wide.get(x_tile, y_tile);
            fine.reset();
            fine.set_coords(x_tile, y_tile);
            fine.clear(wtile.bg.0);
            for cmd in &wtile.cmds {
                fine.run_cmd(cmd, &ctx.alphas, &ctx.encoded_paints);
            }

            let tile_width =
                (width_usize - usize::from(x_tile) * max_tile_width).min(max_tile_width);
            let tile_height =
                (height_usize - usize::from(y_tile) * max_tile_height).min(max_tile_height);
            let blend_buf = fine.blend_buf.last().unwrap();

            for row in 0..tile_height {
                let dest_row =
                    (usize::from(y_tile) * max_tile_height + row) * width_usize * COLOR_COMPONENTS;
                let dest_col = usize::from(x_tile) * max_tile_width * COLOR_COMPONENTS;
                let dest_idx = dest_row + dest_col;

                for col in 0..tile_width {
                    let src_idx = (col * max_tile_height + row) * COLOR_COMPONENTS;
                    let rgba = to_rgba8(
                        &blend_buf[src_idx..src_idx + COLOR_COMPONENTS]
                            .try_into()
                            .unwrap(),
                    );
                    let dest_pixel = dest_idx + col * COLOR_COMPONENTS;
                    buffer[dest_pixel..dest_pixel + COLOR_COMPONENTS].copy_from_slice(&rgba);
                }
            }
        }
    }
}

fn render_tiles_parallel(
    ctx: &RenderContext,
    width: u16,
    height: u16,
    width_tiles: usize,
    height_tiles: usize,
    buffer: &mut [u8],
) {
    if buffer.is_empty() {
        return;
    }

    let width_usize = width as usize;
    let height_usize = height as usize;

    render_tile_rows_recursive(
        ctx,
        width,
        height,
        width_tiles,
        height_tiles,
        buffer,
        0,
        height_tiles,
        width_usize,
        height_usize,
        0,
    );
}

fn render_tile_rows_recursive(
    ctx: &RenderContext,
    width: u16,
    height: u16,
    width_tiles: usize,
    height_tiles: usize,
    buffer: &mut [u8],
    y_start: usize,
    y_end: usize,
    width_usize: usize,
    height_usize: usize,
    row_start_px: usize,
) {
    if y_start >= y_end || buffer.is_empty() {
        return;
    }

    if y_end - y_start <= 1 {
        let y_tile = y_start as u16;
        let max_tile_height = usize::from(Tile::HEIGHT);
        let row_start_px = row_start_px.min(height_usize);
        let row_end_px = (row_start_px + max_tile_height).min(height_usize);
        let actual_rows = row_end_px.saturating_sub(row_start_px);

        render_single_tile_row(
            ctx,
            width,
            height,
            width_tiles,
            y_tile,
            buffer,
            actual_rows,
            width_usize,
        );
    } else {
        let mid = (y_start + y_end) / 2;
        let max_tile_height = usize::from(Tile::HEIGHT);
        let mid_pixel = usize::from(mid)
            .saturating_mul(max_tile_height)
            .min(height_usize);
        let row_stride = width_usize * COLOR_COMPONENTS;
        let rows_in_buffer = buffer.len() / row_stride;
        let buffer_end_px = row_start_px + rows_in_buffer;
        let clamped_mid_pixel = mid_pixel.clamp(row_start_px, buffer_end_px);
        let local_split_rows = clamped_mid_pixel.saturating_sub(row_start_px);
        let split_index = local_split_rows * row_stride;
        let (top, bottom) = buffer.split_at_mut(split_index);

        join(
            || {
                render_tile_rows_recursive(
                    ctx,
                    width,
                    height,
                    width_tiles,
                    height_tiles,
                    top,
                    y_start,
                    mid,
                    width_usize,
                    height_usize,
                    row_start_px,
                )
            },
            || {
                render_tile_rows_recursive(
                    ctx,
                    width,
                    height,
                    width_tiles,
                    height_tiles,
                    bottom,
                    mid,
                    y_end,
                    width_usize,
                    height_usize,
                    clamped_mid_pixel,
                )
            },
        );
    }
}

fn render_single_tile_row(
    ctx: &RenderContext,
    width: u16,
    height: u16,
    width_tiles: usize,
    y_tile: u16,
    row_slice: &mut [u8],
    actual_rows: usize,
    width_usize: usize,
) {
    if actual_rows == 0 {
        return;
    }

    debug_assert_eq!(
        row_slice.len(),
        actual_rows * width_usize * COLOR_COMPONENTS
    );
    let max_tile_width = usize::from(WideTile::WIDTH);
    let max_tile_height = usize::from(Tile::HEIGHT);
    let row_stride = width_usize * COLOR_COMPONENTS;

    with_thread_fine(width, height, |fine| {
        for x_tile in 0..width_tiles as u16 {
            let wtile = ctx.wide.get(x_tile, y_tile);
            fine.reset();
            fine.set_coords(x_tile, y_tile);
            fine.clear(wtile.bg.0);
            for cmd in &wtile.cmds {
                fine.run_cmd(cmd, &ctx.alphas, &ctx.encoded_paints);
            }

            let blend_buf = fine.blend_buf.last().unwrap();
            let tile_width =
                (width_usize - usize::from(x_tile) * max_tile_width).min(max_tile_width);
            let tile_height = actual_rows.min(max_tile_height);
            let col_offset = usize::from(x_tile) * max_tile_width * COLOR_COMPONENTS;

            for row in 0..tile_height {
                let row_offset = row * row_stride;
                for col in 0..tile_width {
                    let src_idx = (col * max_tile_height + row) * COLOR_COMPONENTS;
                    let rgba = to_rgba8(
                        &blend_buf[src_idx..src_idx + COLOR_COMPONENTS]
                            .try_into()
                            .unwrap(),
                    );
                    let dest_idx = row_offset + col_offset + col * COLOR_COMPONENTS;
                    row_slice[dest_idx..dest_idx + COLOR_COMPONENTS].copy_from_slice(&rgba);
                }
            }
        }
    });
}

fn with_thread_fine<R>(width: u16, height: u16, op: impl FnOnce(&mut Fine) -> R) -> R {
    FINE_POOL.with(|pool| {
        let mut pool = pool.borrow_mut();
        let mut fine = pool.pop().unwrap_or_else(|| Fine::new(width, height));
        let result = op(&mut fine);
        fine.reset();
        pool.push(fine);
        result
    })
}

fn configured_threads() -> Option<usize> {
    static THREAD_SETTING: OnceLock<Option<usize>> = OnceLock::new();
    *THREAD_SETTING.get_or_init(|| {
        let value = std::env::var("HAYRO_THREADS").ok()?;
        let trimmed = value.trim();
        if trimmed.is_empty() {
            return None;
        }

        match trimmed.parse::<usize>() {
            Ok(val) if val > 0 => Some(val),
            _ => {
                warn!(
                    "invalid HAYRO_THREADS value `{value}`, falling back to default thread count"
                );
                None
            }
        }
    })
}

fn install_global_pool(threads: usize) {
    static POOL_CONFIGURED: OnceLock<()> = OnceLock::new();
    POOL_CONFIGURED.get_or_init(|| {
        if let Err(err) = ThreadPoolBuilder::new().num_threads(threads).build_global() {
            warn!("failed to configure rayon thread pool for HAYRO_THREADS={threads}: {err}");
        }
    });
}
