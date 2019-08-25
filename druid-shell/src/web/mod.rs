// Copyright 2018 The xi-editor Authors.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! Creation and management of windows.

#![allow(non_snake_case)]

pub mod application;
pub mod dialog;
pub mod menu;
pub mod util;
pub mod win_main;

use std::any::Any;
use std::cell::{Cell, RefCell};
use std::ffi::OsString;
use std::mem;
use std::ptr::{null, null_mut};
use std::rc::{Rc, Weak};
use std::sync::{Arc, Mutex};

use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;
use web_sys::{HtmlCanvasElement};

use piet::RenderContext;

use dialog::{FileDialogOptions, FileDialogType};
use crate::menu::Menu;
use crate::Error;

use crate::window::{self, Cursor, MouseButton, MouseEvent, MouseType, WinHandler};

/// Builder abstraction for creating new windows.
pub struct WindowBuilder {
    handler: Option<Box<WinHandler>>,
    title: String,
    cursor: Cursor,
    menu: Option<Menu>,
}

#[derive(Clone, Default)]
pub struct WindowHandle(Weak<WindowState>);

/// A handle that can get used to schedule an idle handler. Note that
/// this handle is thread safe. If the handle is used after the hwnd
/// has been destroyed, probably not much will go wrong (the XI_RUN_IDLE
/// message may be sent to a stray window).
#[derive(Clone)]
pub struct IdleHandle {
    window_state: WindowHandle,
    queue: Arc<Mutex<Vec<Box<IdleCallback>>>>,
}

//impl Send for IdleHandle { }

trait IdleCallback: Send {
    fn call(self: Box<Self>, a: &Any);
}

impl<F: FnOnce(&Any) + Send> IdleCallback for F {
    fn call(self: Box<F>, a: &Any) {
        (*self)(a)
    }
}

struct WindowState {
    dpi: Cell<f32>,
    dpr: Cell<f64>,
    idle_queue: Arc<Mutex<Vec<Box<IdleCallback>>>>,
    handler: Box<WinHandler>,
    canvas: web_sys::HtmlCanvasElement,
    canvas_ctx: RefCell<web_sys::CanvasRenderingContext2d>,
}

impl WindowState {
    fn render(&self) -> bool {
        let window = window();
        let ref mut canvas_ctx = *self.canvas_ctx.borrow_mut();
        canvas_ctx.clear_rect(0.0, 0.0, self.get_width() as f64, self.get_height() as f64);
        let mut piet_ctx = piet_common::Piet::new(canvas_ctx, &window);
        let want_anim_frame = self.handler.paint(&mut piet_ctx);
        if let Err(e) = piet_ctx.finish() {
            // TODO: use proper log infrastructure
            eprintln!("piet error on render: {:?}", e);
        }
        let res = piet_ctx.finish();
        if let Err(e) = res {
            println!("EndDraw error: {:?}", e);
        }
        want_anim_frame
    }

    fn process_idle_queue(&self) {
        let mut queue = self.idle_queue.lock().expect("process_idle_queue");
        for callback in queue.drain(..) {
            callback.call(&self.handler);
        }
    }

    fn get_width(&self) -> u32 {
        (self.canvas.offset_width() as f64 * self.dpr.get()) as u32
    }

    fn get_height(&self) -> u32 {
        (self.canvas.offset_height() as f64 * self.dpr.get()) as u32
    }
}

fn setup_mouse_down_callback(window_state: &Rc<WindowState>) {
    let state = window_state.clone();
    let closure = Closure::wrap(Box::new(move |event: web_sys::MouseEvent| {
        let button = mouse_button(event.button()).unwrap();
        let dpr = state.dpr.get();
        let event = MouseEvent {
            x: (dpr * event.offset_x() as f64) as i32,
            y: (dpr * event.offset_y() as f64) as i32,
            mods: 0,
            which: button,
            ty: MouseType::Down,
        };
        state.handler.mouse(&event);
    }) as Box<dyn FnMut(_)>);
    window_state.canvas
        .add_event_listener_with_callback("mousedown", closure.as_ref().unchecked_ref())
        .unwrap();
    closure.forget();
}

fn setup_mouse_move_callback(window_state: &Rc<WindowState>) {
    let state = window_state.clone();
    let closure = Closure::wrap(Box::new(move |event: web_sys::MouseEvent| {
        let dpr = state.dpr.get();
        let x = (dpr * event.offset_x() as f64) as i32;
	let y = (dpr * event.offset_y() as f64) as i32;
	let mods = 0;
	state.handler.mouse_move(x, y, mods);
    }) as Box<dyn FnMut(_)>);
    window_state.canvas
        .add_event_listener_with_callback("mousemove", closure.as_ref().unchecked_ref())
        .unwrap();
    closure.forget();
}

fn setup_mouse_up_callback(window_state: &Rc<WindowState>) {
    let state = window_state.clone();
    let closure = Closure::wrap(Box::new(move |event: web_sys::MouseEvent| {
        let button = mouse_button(event.button()).unwrap();
        let dpr = state.dpr.get();
        let event = MouseEvent {
            x: (dpr * event.offset_x() as f64) as i32,
            y: (dpr * event.offset_y() as f64) as i32,
            mods: 0,
            which: button,
            ty: MouseType::Up,
        };
        state.handler.mouse(&event);
    }) as Box<dyn FnMut(_)>);
    window_state.canvas
        .add_event_listener_with_callback("mouseup", closure.as_ref().unchecked_ref()).unwrap();
    closure.forget();
}

fn setup_resize_callback(window_state: &Rc<WindowState>) {
    let state = window_state.clone();
    let closure = Closure::wrap(Box::new(move |_: web_sys::UiEvent| {
        let (css_width, css_height, dpr) = get_window_size_and_dpr();
        let physical_width = (dpr * css_width) as u32;
        let physical_height = (dpr * css_height) as u32;
        state.dpr.replace(dpr);
        state.canvas.set_width(physical_width);
        state.canvas.set_height(physical_height);
        state.handler.size(physical_width, physical_height);
        state.render(); // TODO: this seems a bit mad
    }) as Box<dyn FnMut(_)>);
    window()
        .add_event_listener_with_callback("resize", closure.as_ref().unchecked_ref())
        .unwrap();
    closure.forget();
}
fn setup_web_callbacks(window_state: &Rc<WindowState>) {
    setup_mouse_down_callback(window_state);
    setup_mouse_move_callback(window_state);
    setup_mouse_up_callback(window_state);
    setup_resize_callback(window_state);
}

/// Returns the window size in css units
fn get_window_size_and_dpr() -> (f64, f64, f64) {
    let w = window();
    let width = w.inner_width().unwrap().as_f64().unwrap();
    let height = w.inner_height().unwrap().as_f64().unwrap();
    let dpr = w.device_pixel_ratio();
    (width, height, dpr)
}

impl WindowBuilder {
    pub fn new() -> WindowBuilder {
        WindowBuilder {
            handler: None,
            title: String::new(),
            cursor: Cursor::Arrow,
            menu: None,
        }
    }

    /// This takes ownership, and is typically used with UiMain
    pub fn set_handler(&mut self, handler: Box<WinHandler>) {
        self.handler = Some(handler);
    }

    pub fn set_scroll(&mut self, hscroll: bool, vscroll: bool) {
        // TODO
    }

    pub fn set_title<S: Into<String>>(&mut self, title: S) {
        self.title = title.into();
    }

    /// Set the default cursor for the window.
    pub fn set_cursor(&mut self, cursor: Cursor) {
        self.cursor = cursor;
    }

    pub fn set_menu(&mut self, menu: Menu) {
        self.menu = Some(menu);
    }

    pub fn build(self) -> Result<WindowHandle, Error> {
        let window = window();
        let canvas = window
            .document()
            .unwrap()
            .get_element_by_id("canvas")
            .unwrap()
            .dyn_into::<HtmlCanvasElement>()
            .unwrap();
        let mut context = canvas
            .get_context("2d")
            .unwrap()
            .unwrap()
            .dyn_into::<web_sys::CanvasRenderingContext2d>()
            .unwrap();

        let dpr = window.device_pixel_ratio();
        let old_w = canvas.offset_width();
        let old_h = canvas.offset_height();
        let new_w = (old_w as f64 * dpr) as u32;
        let new_h = (old_h as f64 * dpr) as u32;

        // canvas.style().set_property("width", &old_w.to_string()).unwrap();
        // canvas.style().set_property("height", &old_h.to_string()).unwrap();

        canvas.set_width(new_w);
        canvas.set_height(new_h);
        // let _ = context.scale(dpr, dpr);

        let handler = self.handler.unwrap();

        handler.size(new_w, new_h);

        let window = Rc::new(WindowState {
            dpi: Cell::new(96.0),
            dpr: Cell::new(dpr),
            idle_queue: Default::default(),
            handler: handler,
            canvas: canvas,
            canvas_ctx: RefCell::new(context),
        });

        setup_web_callbacks(&window);

        let handle = WindowHandle(Rc::downgrade(&window));

        window.handler.connect(&window::WindowHandle {
            inner: handle.clone(),
        });

        Ok(handle)
    }
}

impl WindowHandle {
    pub fn show(&self) {
        self.render_soon();
    }

    pub fn close(&self) {
        // TODO
    }

    pub fn invalidate(&self) {
        self.render_soon();
    }

    fn render_soon(&self) {
        if let Some(w) = self.0.upgrade() {
            let handle = self.clone();
            request_animation_frame(move || {
                let want_anim_frame = w.render();
                if want_anim_frame {
                    handle.render_soon();
                }
            });
        }
    }

    pub fn file_dialog(
        &self,
        ty: FileDialogType,
        options: FileDialogOptions,
    ) -> Result<OsString, Error> {
        Err(Error::Null)
    }

    /// Get a handle that can be used to schedule an idle task.
    pub fn get_idle_handle(&self) -> Option<IdleHandle> {
        self.0.upgrade().map(|w| IdleHandle {
            window_state: self.clone(),
            queue: w.idle_queue.clone(),
        })
    }

    fn take_idle_queue(&self) -> Vec<Box<IdleCallback>> {
        if let Some(w) = self.0.upgrade() {
            mem::replace(&mut w.idle_queue.lock().unwrap(), Vec::new())
        } else {
            Vec::new()
        }
    }

    /// Get the dpi of the window.
    pub fn get_dpi(&self) -> f32 {
        if let Some(w) = self.0.upgrade() {
            w.dpi.get()
        } else {
            96.0
        }
    }

    /// Convert a dimension in px units to physical pixels (rounding).
    pub fn px_to_pixels(&self, x: f32) -> i32 {
        (x * self.get_dpi() * (1.0 / 96.0)).round() as i32
    }

    /// Convert a point in px units to physical pixels (rounding).
    pub fn px_to_pixels_xy(&self, x: f32, y: f32) -> (i32, i32) {
        let scale = self.get_dpi() * (1.0 / 96.0);
        ((x * scale).round() as i32, (y * scale).round() as i32)
    }

    /// Convert a dimension in physical pixels to px units.
    pub fn pixels_to_px<T: Into<f64>>(&self, x: T) -> f32 {
        (x.into() as f32) * 96.0 / self.get_dpi()
    }

    /// Convert a point in physical pixels to px units.
    pub fn pixels_to_px_xy<T: Into<f64>>(&self, x: T, y: T) -> (f32, f32) {
        let scale = 96.0 / self.get_dpi();
        ((x.into() as f32) * scale, (y.into() as f32) * scale)
    }
}

unsafe impl Send for IdleHandle {}

impl IdleHandle {
    /// Add an idle handler, which is called (once) when the main thread is idle.
    pub fn add_idle<F>(&self, callback: F)
    where
        F: FnOnce(&Any) + Send + 'static,
    {
        let mut queue = self.queue.lock().expect("IdleHandle::add_idle queue");
        queue.push(Box::new(callback));

        if queue.len() == 1 {
            let window_state = self.window_state.0.clone();
            request_animation_frame(move || {
                if let Some(w) = window_state.upgrade() {
                    w.process_idle_queue();
                }
            });
        }
    }
}

fn window() -> web_sys::Window { util::window() }

fn request_animation_frame(f: impl FnOnce() + 'static) {
    window()
        .request_animation_frame(Closure::once_into_js(f).as_ref().unchecked_ref())
        .expect("druid_shell web_sys::Window::request_animation_frame");
}

fn mouse_button(button: i16) -> Option<MouseButton> {
    match button {
        0 => Some(MouseButton::Left),
        1 => Some(MouseButton::Middle),
        2 => Some(MouseButton::Right),
        _ => None
    }
}
