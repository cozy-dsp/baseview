use std::cell::RefCell;
use std::marker::PhantomData;
use std::rc::Rc;

use raw_window_handle::{
    DisplayHandle, HandleError, HasDisplayHandle, HasRawDisplayHandle, HasRawWindowHandle,
    HasWindowHandle, RawDisplayHandle, RawWindowHandle,
};

use crate::event::{Event, EventStatus};
use crate::window_open_options::WindowOpenOptions;
use crate::{MouseCursor, Point, Size};

#[cfg(target_os = "macos")]
use crate::macos as platform;
#[cfg(target_os = "windows")]
use crate::win as platform;
#[cfg(target_os = "linux")]
use crate::x11 as platform;

pub struct WindowHandle {
    window_handle: platform::WindowHandle,
    // so that WindowHandle is !Send on all platforms
    phantom: PhantomData<*mut ()>,
}

impl WindowHandle {
    fn new(window_handle: platform::WindowHandle) -> Self {
        Self { window_handle, phantom: PhantomData }
    }

    /// Close the window
    pub fn close(&mut self) {
        self.window_handle.close();
    }

    /// Returns `true` if the window is still open, and returns `false`
    /// if the window was closed/dropped.
    pub fn is_open(&self) -> bool {
        self.window_handle.is_open()
    }
}

impl HasWindowHandle for WindowHandle {
    fn window_handle(&self) -> Result<raw_window_handle::WindowHandle<'_>, HandleError> {
        self.window_handle.window_handle()
    }
}

pub trait WindowHandler {
    fn on_frame(&mut self, window: Window);
    fn on_event(&mut self, window: Window, event: Event) -> EventStatus;
}

#[derive(Clone)]
pub struct Window {
    window: platform::Window,
    // so that Window is !Send on all platforms
    phantom: PhantomData<*mut ()>,
}

impl Window {
    pub(crate) fn new(window: platform::Window) -> Window {
        Window { window, phantom: PhantomData }
    }

    pub fn open_parented<P, H, B>(parent: &P, options: WindowOpenOptions, build: B) -> WindowHandle
    where
        P: HasWindowHandle,
        H: WindowHandler + 'static,
        B: FnOnce(Window) -> H,
        B: Send + 'static,
    {
        let window_handle = platform::Window::open_parented::<P, H, B>(parent, options, build);
        WindowHandle::new(window_handle)
    }

    pub fn open_blocking<H, B>(options: WindowOpenOptions, build: B)
    where
        H: WindowHandler + 'static,
        B: FnOnce(crate::Window) -> H,
        B: Send + 'static,
    {
        platform::Window::open_blocking::<H, B>(options, build)
    }

    /// Close the window
    pub fn close(&mut self) {
        self.window.close();
    }

    /// Resize the window to the given size. The size is always in logical pixels. DPI scaling will
    /// automatically be accounted for.
    pub fn resize(&mut self, size: Size) {
        self.window.resize(size);
    }

    pub fn set_mouse_cursor(&mut self, cursor: MouseCursor) {
        self.window.set_mouse_cursor(cursor);
    }

    pub fn set_mouse_position(&mut self, point: Point) {
        self.window.set_mouse_position(point);
    }

    pub fn has_focus(&mut self) -> bool {
        self.window.has_focus()
    }

    pub fn focus(&mut self) {
        self.window.focus()
    }

    /// If provided, then an OpenGL context will be created for this window. You'll be able to
    /// access this context through [crate::Window::gl_context].
    #[cfg(feature = "opengl")]
    pub fn gl_context(&self) -> Option<&crate::gl::GlContext> {
        self.window.gl_context()
    }
}

impl HasWindowHandle for Window {
    fn window_handle(&self) -> Result<raw_window_handle::WindowHandle<'_>, HandleError> {
        self.window.window_handle()
    }
}

impl HasDisplayHandle for Window {
    fn display_handle(&self) -> Result<DisplayHandle<'_>, HandleError> {
        self.window.display_handle()
    }
}
