// Copyright 2019 Google LLC
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     https://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

#![feature(arbitrary_self_types)]
#![recursion_limit = "1000"]
#![feature(vec_remove_item)]
#![feature(drain_filter)]
#![feature(stmt_expr_attributes)]

#[macro_use]
extern crate log;
#[macro_use]
extern crate serde_derive;
#[macro_use]
extern crate failure;
#[macro_use]
extern crate lazy_static;

mod database;
mod image;
mod stats;
mod vec;
mod view;

use crate::stats::ScopedDuration;
use boolinator::Boolinator;
use clap::Arg;
use futures::future::Fuse;
use futures::future::FutureExt;
use futures::future::RemoteHandle;
use futures::select;
use futures::task::SpawnExt;
use piston_window::*;
use std::cmp::Ordering;
use std::collections::{BTreeMap, VecDeque};
use std::sync::Arc;
use vec::*;

#[derive(Debug, Fail)]
pub enum E {
    #[fail(display = "rocksdb error: {:?}", 0)]
    RocksError(rocksdb::Error),

    #[fail(display = "decode error {:?}", 0)]
    DecodeError(bincode::Error),

    #[fail(display = "encode error {:?}", 0)]
    EncodeError(bincode::Error),

    #[fail(display = "missing data for key {:?}", 0)]
    MissingData(String),

    #[fail(display = "image error: {:?}", 0)]
    ImageError(::image::ImageError),
}

type R<T> = std::result::Result<T, E>;

#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
struct Pow2(u8);

impl Pow2 {
    fn from(i: u32) -> Self {
        assert!(i.is_power_of_two());
        Pow2((32 - i.leading_zeros() - 1) as u8)
    }

    #[allow(unused)]
    fn u32(&self) -> u32 {
        1 << self.0
    }
}

#[test]
fn size_conversions() {
    assert_eq!(Pow2::from(128), Pow2(7));
    assert_eq!(Pow2(7).u32(), 128);
}

#[derive(
    Debug, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord, Hash, Copy, Clone, Default,
)]
pub struct TileRef(u64);

impl TileRef {
    fn new(size: Pow2, index: u64, chunk: u16) -> Self {
        Self((chunk as u64) | ((index % (1u64 << 40)) << 16) | ((size.0 as u64) << 56))
    }

    #[cfg(test)]
    fn deconstruct(&self) -> (Pow2, u64, u16) {
        let size = ((self.0 & 0xFF00_0000_0000_0000u64) >> 56) as u8;
        let index = (self.0 & 0x00FF_FFFF_FFFF_0000u64) >> 16;
        let chunk = (self.0 & 0x0000_0000_0000_FFFFu64) as u16;
        (Pow2(size), index, chunk)
    }
}

#[test]
fn tile_ref_test() {
    assert_eq!(
        TileRef::new(Pow2(0xFFu8), 0u64, 0u16),
        TileRef(0xFF00_0000_0000_0000u64)
    );
    assert_eq!(
        TileRef::new(Pow2(0xFFu8), 0u64, 0u16).deconstruct(),
        (Pow2(0xFFu8), 0u64, 0u16)
    );
    assert_eq!(
        TileRef::new(Pow2(0xFFu8), 0u64, 0u16).0.to_be_bytes(),
        [0xFF, 0, 0, 0, 0, 0, 0, 0]
    );

    assert_eq!(
        TileRef::new(Pow2(0u8), 0x00FF_FFFF_FFFFu64, 0u16),
        TileRef(0x00F_FFFFF_FFFF_0000u64)
    );
    assert_eq!(
        TileRef::new(Pow2(0u8), 0x00FF_FFFF_FFFFu64, 0u16).deconstruct(),
        (Pow2(0u8), 0x00FF_FFFF_FFFFu64, 0u16)
    );

    assert_eq!(
        TileRef::new(Pow2(0u8), 0u64, 0xFFFFu16),
        TileRef(0x0000_0000_0000_FFFFu64)
    );
    assert_eq!(
        TileRef::new(Pow2(0u8), 0u64, 0xFFFFu16).deconstruct(),
        (Pow2(0u8), 0u64, 0xFFFFu16)
    )
}

#[derive(Debug, Serialize, Deserialize)]
struct Thumb {
    img_size: [u32; 2],
    tile_refs: Vec<TileRef>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Metadata {
    thumbs: Vec<Thumb>,
}

impl Metadata {
    fn nearest(&self, target_size: u32) -> usize {
        let mut found = None;

        let ts_zeros = target_size.leading_zeros() as i16;

        for (i, thumb) in self.thumbs.iter().enumerate() {
            let size = thumb.size();
            let size_zeros = size.leading_zeros() as i16;
            let dist = (ts_zeros - size_zeros).abs();
            if let Some((found_dist, found_i)) = found.take() {
                if dist < found_dist {
                    found = Some((dist, i));
                } else {
                    found = Some((found_dist, found_i));
                }
            } else {
                found = Some((dist, i));
            }
        }

        let (_, i) = found.unwrap();
        i
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct TileSpec {
    img_size: [u32; 2],

    // Grid width and height (in number of tiles).
    grid_size: [u32; 2],

    // Tile width and height in pixels.
    tile_size: [u32; 2],
}

impl TileSpec {
    fn ranges(img_size: u32, grid_size: u32, tile_size: u32) -> impl Iterator<Item = (u32, u32)> {
        (0..grid_size).map(move |i| {
            let min = i * tile_size;
            let max = std::cmp::min(img_size, min + tile_size);
            (min, max)
        })
    }

    fn x_ranges(&self) -> impl Iterator<Item = (u32, u32)> {
        Self::ranges(self.img_size[0], self.grid_size[0], self.tile_size[0])
    }

    fn y_ranges(&self) -> impl Iterator<Item = (u32, u32)> {
        Self::ranges(self.img_size[1], self.grid_size[1], self.tile_size[1])
    }
}

impl Thumb {
    fn max_dimension(&self) -> u32 {
        let [w, h] = self.img_size;
        std::cmp::max(w, h)
    }

    fn size(&self) -> u32 {
        self.max_dimension().next_power_of_two()
    }

    fn tile_spec(&self) -> TileSpec {
        let img_size = vec2_f64(self.img_size);
        let tile_size = vec2_scale(vec2_log(img_size, 8.0), 128.0);
        let grid_size = vec2_ceil(vec2_div(img_size, tile_size));
        let tile_size = vec2_ceil(vec2_div(img_size, grid_size));
        TileSpec {
            img_size: self.img_size,
            grid_size: vec2_u32(grid_size),
            tile_size: vec2_u32(tile_size),
        }
    }
}

impl Draw for Thumb {
    fn draw(
        &self,
        trans: [[f64; 3]; 2],
        zoom: f64,
        tiles: &BTreeMap<TileRef, G2dTexture>,
        draw_state: &DrawState,
        g: &mut G2d,
    ) -> bool {
        let img = piston_window::image::Image::new();

        let max_dimension = self.max_dimension() as f64;

        let trans = trans.zoom(zoom / max_dimension);

        // Center the image within the grid square.
        let [x_offset, y_offset] = {
            let img_size = vec2_f64(self.img_size);
            let gaps = vec2_sub([max_dimension, max_dimension], img_size);
            vec2_scale(gaps, 0.5)
        };

        let tile_spec = self.tile_spec();

        let mut it = self.tile_refs.iter();
        for (y, _) in tile_spec.y_ranges() {
            for (x, _) in tile_spec.x_ranges() {
                let tile_ref = it.next().unwrap();
                if let Some(texture) = tiles.get(tile_ref) {
                    let trans = trans.trans(x_offset + x as f64, y_offset + y as f64);
                    img.draw(texture, &draw_state, trans, g);
                }
            }
        }

        true
    }
}

static UPS: u64 = 100;

static UPSIZE_FACTOR: f64 = 1.5;

trait Draw {
    fn draw(
        &self,
        trans: [[f64; 3]; 2],
        zoom: f64,
        tiles: &BTreeMap<TileRef, G2dTexture>,
        draw_state: &DrawState,
        g: &mut G2d,
    ) -> bool;
}

#[derive(Debug)]
pub enum MetadataState {
    Missing,
    Some(Metadata),
    Errored,
}

impl std::default::Default for MetadataState {
    fn default() -> Self {
        MetadataState::Missing
    }
}

type Handle<T> = Fuse<RemoteHandle<T>>;

struct App {
    db: Arc<database::Database>,

    images: Vec<image::Image>,

    // Graphics state
    new_window_settings: Option<WindowSettings>,
    window_settings: WindowSettings,
    window: PistonWindow,
    texture_context: G2dTextureContext,

    tiles: BTreeMap<TileRef, G2dTexture>,

    // Movement state & modes.
    view: view::View,
    panning: bool,
    zooming: Option<f64>,
    cursor_captured: bool,

    // Mouse distance calculations are relative to this point.
    focus: Option<Vector2<f64>>,

    cache_todo: [VecDeque<usize>; 2],

    thumb_todo: [VecDeque<usize>; 2],
    thumb_handles: BTreeMap<usize, Handle<image::ThumbRet>>,
    thumb_executor: futures::executor::ThreadPool,
    thumb_threads: usize,

    shift_held: bool,

    base_id: u64,
}

struct Stopwatch {
    start: std::time::Instant,
    duration: std::time::Duration,
}

impl Stopwatch {
    fn from_millis(millis: u64) -> Self {
        Self {
            start: std::time::Instant::now(),
            duration: std::time::Duration::from_millis(millis),
        }
    }

    fn done(&self) -> bool {
        self.start.elapsed() >= self.duration
    }
}

impl App {
    fn new(
        images: Vec<image::Image>,
        db: Arc<database::Database>,
        thumbnailer_threads: usize,
        base_id: u64,
    ) -> Self {
        let view = view::View::new(images.len());

        let window_settings = WindowSettings::new("pix", [800.0, 600.0])
            .exit_on_esc(true)
            .fullscreen(false);

        let mut window: PistonWindow = window_settings.build().expect("window build");
        window.set_ups(UPS);

        let texture_context = window.create_texture_context();

        Self {
            db,

            new_window_settings: None,
            window_settings,
            window,
            texture_context,

            tiles: BTreeMap::new(),

            view,
            panning: false,
            zooming: None,
            cursor_captured: false,

            cache_todo: [
                VecDeque::with_capacity(images.len()),
                VecDeque::with_capacity(images.len()),
            ],

            thumb_handles: BTreeMap::new(),
            thumb_executor: futures::executor::ThreadPool::builder()
                .pool_size(thumbnailer_threads)
                .name_prefix("thumbnailer")
                .create()
                .unwrap(),
            thumb_threads: thumbnailer_threads,

            thumb_todo: [
                VecDeque::with_capacity(images.len()),
                VecDeque::with_capacity(images.len()),
            ],

            shift_held: false,

            focus: None,

            base_id,

            images,
        }
    }

    fn rebuild_window(&mut self, settings: WindowSettings) {
        for image in &mut self.images {
            image.reset();
        }

        self.window_settings = settings.clone();
        self.window = settings.build().expect("window build");

        self.tiles.clear();
        self.focus = None;
        self.panning = false;
        self.cursor_captured = false;
        self.zooming = None;
    }

    fn target_size(&self) -> u32 {
        ((self.view.zoom * UPSIZE_FACTOR) as u32).next_power_of_two()
    }

    fn load_cache(&mut self, stopwatch: &Stopwatch) {
        let _s = ScopedDuration::new("load_tile_from_db");

        let target_size = self.target_size();

        let texture_settings = TextureSettings::new();

        // visible first
        for p in 0..self.cache_todo.len() {
            while let Some(i) = self.cache_todo[p].pop_front() {
                let image = &self.images[i];

                let metadata = match &image.metadata {
                    MetadataState::Missing => {
                        self.thumb_todo[p].push_back(i);
                        continue;
                    }
                    MetadataState::Some(metadata) => metadata,
                    MetadataState::Errored => continue,
                };

                let shift = if p == 0 {
                    0
                } else {
                    let ratio = self.view.visible_ratio(self.view.coords(i));
                    f64::max(0.0, ratio - 1.0).floor() as usize
                };

                let new_size = metadata.nearest(target_size >> shift);

                let current_size = image.size.unwrap_or(0);

                // Progressive resizing.
                let new_size = match new_size.cmp(&current_size) {
                    Ordering::Less => current_size - 1,
                    Ordering::Equal => {
                        // Already loaded target size.
                        continue;
                    }
                    Ordering::Greater => current_size + 1,
                };

                // Load new tiles.
                for tile_ref in &metadata.thumbs[new_size].tile_refs {
                    // Already loaded.
                    if self.tiles.contains_key(tile_ref) {
                        continue;
                    }

                    if stopwatch.done() {
                        self.cache_todo[p].push_front(i);
                        return;
                    }

                    // load the tile from the cache
                    let _s3 = ScopedDuration::new("load_tile");

                    let data = self
                        .db
                        .get(*tile_ref)
                        .expect("db get")
                        .expect("missing tile");

                    let image = ::image::load_from_memory(&data).expect("load image");

                    // TODO: Would be great to move off thread.
                    let image = Texture::from_image(
                        &mut self.texture_context,
                        &image.to_rgba(),
                        &texture_settings,
                    )
                    .expect("texture");

                    self.tiles.insert(*tile_ref, image);
                }

                // Unload old tiles.
                for (j, thumb) in metadata.thumbs.iter().enumerate() {
                    if j == new_size {
                        continue;
                    }
                    for tile_ref in &thumb.tile_refs {
                        self.tiles.remove(tile_ref);
                    }
                }

                self.images[i].size = Some(new_size);

                self.cache_todo[p].push_back(i);
            }
        }
    }

    fn make_thumb(&mut self, i: usize) {
        let image = &self.images[i];

        if !image.is_missing() {
            return;
        }

        if self.thumb_handles.contains_key(&i) {
            return;
        }

        let tile_id_index = self.base_id + i as u64;

        let fut = image.make_thumb(tile_id_index, Arc::clone(&self.db));

        let handle = self.thumb_executor.spawn_with_handle(fut).unwrap().fuse();

        self.thumb_handles.insert(i, handle);
    }

    fn make_thumbs(&mut self) {
        let _s = ScopedDuration::new("make_thumbs");

        for p in 0..self.thumb_todo.len() {
            while let Some(i) = {
                if self.thumb_handles.len() > self.thumb_threads {
                    return;
                }
                self.thumb_todo[p].pop_front()
            } {
                self.make_thumb(i);
            }
        }
    }

    fn recv_thumbs(&mut self) {
        let _s = ScopedDuration::new("recv_thumbs");

        let mut done: Vec<usize> = Vec::new();

        let mut handles = BTreeMap::new();
        std::mem::swap(&mut handles, &mut self.thumb_handles);

        for (&i, mut handle) in &mut handles {
            select! {
                thumb_res = handle => {
                    self.images[i].metadata = match thumb_res {
                        Ok(metadata) => {
                            self.cache_todo[self.pri(i)].push_front(i);
                            MetadataState::Some(metadata)
                        }
                        Err(e) => {
                            error!("make_thumb: {}", e);
                            MetadataState::Errored
                        }
                    };

                    done.push(i);
                }

                default => {}
            }
        }

        for i in &done {
            handles.remove(i);
        }

        std::mem::swap(&mut handles, &mut self.thumb_handles);
    }

    fn update(&mut self, args: UpdateArgs) {
        let _s = ScopedDuration::new("update");
        let stopwatch = Stopwatch::from_millis(10);

        if let Some(z) = self.zooming {
            self.zoom(z.mul_add(args.dt, 1.0));
        }

        if self.focus.is_none() {
            self.recalc_visible();
            self.focus = Some(vec2_add(self.view.coords(0), self.view.mouse()));
        }

        self.recv_thumbs();
        self.make_thumbs();

        self.load_cache(&stopwatch);
    }

    fn resize(&mut self, win_size: Vector2<u32>) {
        let _s = ScopedDuration::new("resize");
        self.view.resize_to(win_size);
        self.focus = None;
    }

    fn recalc_visible(&mut self) {
        let _s = ScopedDuration::new("recalc_visible");

        for q in &mut self.cache_todo {
            q.clear();
        }

        for q in &mut self.thumb_todo {
            q.clear();
        }

        let mut mouse_distance: Vec<usize> = self
            .images
            .iter()
            .enumerate()
            .filter_map(|(i, image)| if image.loadable() { Some(i) } else { None })
            .collect();

        mouse_distance.sort_by_key(|&i| vec2_square_len(self.view.mouse_dist(i)) as isize);

        for i in mouse_distance {
            self.cache_todo[self.pri(i)].push_back(i);
        }
    }

    fn pri(&self, i: usize) -> usize {
        !self.view.is_visible(self.view.coords(i)) as usize
    }

    fn mouse_move(&mut self, loc: Vector2<f64>) {
        self.view.mouse_to(loc);
        self.maybe_refocus();
    }

    fn force_refocus(&mut self) {
        self.focus = None;
    }

    fn maybe_refocus(&mut self) {
        if let Some(old) = self.focus {
            let new = self.view.mouse_dist(0);
            let delta = vec2_sub(new, old);
            if vec2_square_len(delta) > 500.0 {
                self.force_refocus();
            }
        }
    }

    fn mouse_zoom(&mut self, v: f64) {
        let _s = ScopedDuration::new("mouse_zoom");
        for _ in 0..(v as isize) {
            self.zoom(1.0 + self.zoom_increment());
        }
        for _ in (v as isize)..0 {
            self.zoom(1.0 - self.zoom_increment());
        }
    }

    fn mouse_pan(&mut self, delta: Vector2<f64>) {
        if self.panning {
            let _s = ScopedDuration::new("mouse_pan");
            if self.cursor_captured {
                self.view.center_mouse();
            }
            self.trans(vec2_scale(delta, 4.0));
        }
    }

    fn shift_increment(&self) -> f64 {
        if self.shift_held {
            // snap to zoom
            if self.view.zoom > 100.0 {
                self.view.zoom
            } else {
                100.0
            }
        } else {
            20.0
        }
    }

    fn zoom_increment(&self) -> f64 {
        if self.shift_held {
            0.5
        } else {
            0.1
        }
    }

    fn trans(&mut self, trans: Vector2<f64>) {
        self.view.trans_by(trans);
        self.maybe_refocus();
    }

    fn zoom(&mut self, ratio: f64) {
        self.view.zoom_by(ratio);
        self.maybe_refocus();
    }

    fn reset(&mut self) {
        self.view.reset();
        self.force_refocus();
    }

    fn button(&mut self, b: ButtonArgs) {
        let _s = ScopedDuration::new("button");
        match (b.state, b.button) {
            (ButtonState::Press, Button::Keyboard(Key::Z)) => {
                self.reset();
            }

            (ButtonState::Press, Button::Keyboard(Key::F)) => {
                let mut settings = self.window_settings.clone();
                settings.set_fullscreen(!settings.get_fullscreen());
                self.new_window_settings = Some(settings);
            }

            (ButtonState::Press, Button::Keyboard(Key::T)) => {
                self.cursor_captured = !self.cursor_captured;
                self.window.set_capture_cursor(self.cursor_captured);
                self.panning = self.cursor_captured;
                self.view.center_mouse();
            }

            (ButtonState::Press, Button::Keyboard(Key::Up)) => {
                self.trans([0.0, self.shift_increment()]);
            }

            (ButtonState::Press, Button::Keyboard(Key::Down)) => {
                self.trans([0.0, -self.shift_increment()]);
            }

            (ButtonState::Press, Button::Keyboard(Key::Left)) => {
                self.trans([self.shift_increment(), 0.0]);
            }

            (ButtonState::Press, Button::Keyboard(Key::Right)) => {
                self.trans([-self.shift_increment(), 0.0]);
            }

            (ButtonState::Press, Button::Keyboard(Key::PageUp)) => {
                self.view.center_mouse();
                self.zoom(1.0 - self.zoom_increment());
            }

            (ButtonState::Press, Button::Keyboard(Key::PageDown)) => {
                self.view.center_mouse();
                self.zoom(1.0 + self.zoom_increment());
            }

            (state, Button::Keyboard(Key::LShift)) | (state, Button::Keyboard(Key::RShift)) => {
                self.shift_held = state == ButtonState::Press;
            }

            (state, Button::Mouse(MouseButton::Middle)) => {
                self.panning = state == ButtonState::Press;
            }

            (state, Button::Mouse(MouseButton::Left)) => {
                self.zooming = (state == ButtonState::Press).as_some(5.0);
            }

            (state, Button::Mouse(MouseButton::Right)) => {
                self.zooming = (state == ButtonState::Press).as_some(-5.0);
            }

            _ => {}
        }
    }

    fn draw_2d(
        thumb_handles: &BTreeMap<usize, Handle<image::ThumbRet>>,
        e: &Event,
        c: Context,
        g: &mut G2d,
        view: &view::View,
        tiles: &BTreeMap<TileRef, G2dTexture>,
        images: &[image::Image],
    ) {
        clear([0.0, 0.0, 0.0, 1.0], g);

        let args = e.render_args().expect("render args");
        let draw_state = DrawState::default().scissor([0, 0, args.draw_size[0], args.draw_size[1]]);

        let black = color::hex("000000");
        let missing_color = color::hex("888888");
        let op_color = color::hex("222222");

        let zoom = (view.zoom * view.zoom) / (view.zoom + 1.0);

        for (i, image) in images.iter().enumerate() {
            let [x, y] = view.coords(i);

            if !view.is_visible([x, y]) {
                continue;
            }

            let trans = c.transform.trans(x, y);

            if image.draw(trans, zoom, tiles, &draw_state, g) {
                continue;
            }

            if thumb_handles.contains_key(&i) {
                rectangle(op_color, [0.0, 0.0, zoom, zoom], trans, g);
                rectangle(black, [1.0, 1.0, zoom - 2.0, zoom - 2.0], trans, g);
            } else {
                rectangle(missing_color, [zoom / 2.0, zoom / 2.0, 1.0, 1.0], trans, g);
            }
        }
    }

    fn run(&mut self) {
        loop {
            let _s = ScopedDuration::new("run_loop");

            if let Some(settings) = self.new_window_settings.take() {
                self.rebuild_window(settings);
            }

            if let Some(e) = self.window.next() {
                let _s = ScopedDuration::new("run_loop_next");

                e.update(|args| {
                    self.update(*args);
                });

                e.resize(|args| {
                    self.resize(args.draw_size);
                });

                e.mouse_scroll(|[_, v]| {
                    self.mouse_zoom(v);
                });

                e.mouse_cursor(|loc| {
                    self.mouse_move(loc);
                });

                e.mouse_relative(|delta| {
                    self.mouse_pan(delta);
                });

                e.button(|b| self.button(b));

                // borrowck
                let v = &self.view;
                let t = &self.tiles;
                let images = &self.images;
                let thumb_handles = &self.thumb_handles;
                self.window.draw_2d(&e, |c, g, _device| {
                    let _s = ScopedDuration::new("draw_2d");
                    Self::draw_2d(thumb_handles, &e, c, g, v, t, images);
                });
            } else {
                break;
            }
        }

        self.thumb_handles.clear();
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Default)]
pub struct File {
    path: String,
    modified: u64,
    file_size: u64,
}

fn find_images(dirs: Vec<String>) -> Vec<Arc<File>> {
    let _s = ScopedDuration::new("find_images");

    let mut ret = Vec::new();

    for dir in dirs {
        for entry in walkdir::WalkDir::new(&dir) {
            let i = ret.len();
            if i > 0 && i % 1000 == 0 {
                info!("Found {} images...", i);
            }

            let entry = match entry {
                Ok(entry) => entry,
                Err(e) => {
                    error!("Walkdir error: {:?}", e);
                    continue;
                }
            };

            let metadata = match entry.metadata() {
                Ok(metadata) => metadata,
                Err(e) => {
                    error!("Metadata lookup error: {:?}: {:?}", entry, e);
                    continue;
                }
            };

            if metadata.is_dir() {
                info!("Searching in {:?}", entry.path());
                continue;
            }

            let file_size = metadata.len();

            let modified: u64 = metadata
                .modified()
                .expect("metadata modified")
                .duration_since(std::time::SystemTime::UNIX_EPOCH)
                .expect("duration since unix epoch")
                .as_secs();

            let path = entry.path().canonicalize().expect("canonicalize");
            let path = if let Some(path) = path.to_str() {
                path.to_owned()
            } else {
                error!("Skipping non-utf8 path: {:?}", path);
                continue;
            };

            let file = File {
                path,
                modified,
                file_size,
            };

            ret.push(Arc::new(file));
        }
    }

    ret.sort();
    ret
}

fn main() {
    env_logger::init();

    /////////////////
    // PARSE FLAGS //
    /////////////////

    let matches = clap::App::new("pix")
        .version("1.0")
        .author("Mason Larobina <mason.larobina@gmail.com>")
        .arg(
            Arg::with_name("paths")
                .value_name("PATHS")
                .multiple(true)
                .help("Images or directories of images to view."),
        )
        .arg(
            Arg::with_name("threads")
                .long("--threads")
                .value_name("COUNT")
                .takes_value(true)
                .required(false)
                .help("Set number of background thumbnailer threads."),
        )
        .arg(
            Arg::with_name("db_path")
                .long("--db_path")
                .value_name("PATH")
                .takes_value(true)
                .help("Alternate thumbnail database path."),
        )
        .get_matches();

    let paths = matches
        .values_of_lossy("paths")
        .unwrap_or_else(|| vec![String::from(".")]);
    info!("Paths: {:?}", paths);

    let thumbnailer_threads: usize = if let Some(threads) = matches.value_of("threads") {
        threads.parse().expect("not an int")
    } else {
        num_cpus::get()
    };
    info!("Thumbnailer threads {}", thumbnailer_threads);

    let db_path: String = if let Some(db_path) = matches.value_of("db_path") {
        db_path.to_owned()
    } else {
        let mut db_path = dirs::cache_dir().expect("cache dir");
        db_path.push("pix/thumbs.db");
        db_path.to_str().expect("db path as str").to_owned()
    };
    info!("Database path: {}", db_path);

    /////////
    // RUN //
    /////////

    let files = find_images(paths);

    assert!(!files.is_empty());
    info!("Found {} images", files.len());

    let db = database::Database::open(&db_path).expect("db open");
    let base_id = db.reserve(files.len());

    let images: Vec<image::Image> = files
        .into_iter()
        .map(|file| {
            let metadata = match db.get_metadata(&*file) {
                Ok(Some(metadata)) => Some(metadata),
                Err(e) => {
                    error!("get metadata error: {:?}", e);
                    None
                }
                _ => None,
            };

            image::Image::from(file, metadata)
        })
        .collect();

    {
        let _s = ScopedDuration::new("uptime");
        App::new(images, Arc::new(db), thumbnailer_threads, base_id).run();
    }

    stats::dump();
}
