use std::cell::Cell;
use std::error::Error;
use std::ffi::c_void;
use std::ptr::NonNull;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, sync_channel, Receiver, SyncSender};
use std::sync::Arc;
use std::thread;

use raw_window_handle::{
    DisplayHandle, HandleError, HasDisplayHandle, HasRawDisplayHandle, HasRawWindowHandle,
    HasWindowHandle, RawDisplayHandle, RawWindowHandle, XlibDisplayHandle, XlibWindowHandle,
};

use x11rb::connection::Connection;
use x11rb::protocol::xproto::{
    AtomEnum, ChangeWindowAttributesAux, ConfigureWindowAux, ConnectionExt as _, CreateGCAux,
    CreateWindowAux, EventMask, PropMode, Visualid, Window as XWindow, WindowClass,
};
use x11rb::wrapper::ConnectionExt as _;

use super::XcbConnection;
use crate::{
    Event, MouseCursor, Point, Size, WindowEvent, WindowHandler, WindowInfo, WindowOpenOptions,
    WindowScalePolicy,
};

#[cfg(feature = "opengl")]
use crate::gl::{platform, GlContext};
use crate::x11::event_loop::EventLoop;
use crate::x11::visual_info::WindowVisualConfig;

pub struct WindowHandle {
    raw_window_handle: Option<RawWindowHandle>,
    close_requested: SyncSender<()>,
    is_open: Arc<AtomicBool>,
}

impl WindowHandle {
    pub fn close(&mut self) {
        self.close_requested.send(()).ok();
    }

    pub fn is_open(&self) -> bool {
        self.is_open.load(Ordering::Relaxed)
    }
}

impl HasWindowHandle for WindowHandle {
    fn window_handle(&self) -> Result<raw_window_handle::WindowHandle<'_>, HandleError> {
        self.raw_window_handle
            .ok_or(HandleError::Unavailable)
            .map(|rwh| unsafe { raw_window_handle::WindowHandle::borrow_raw(rwh) })
    }
}

pub(crate) struct ParentHandle {
    close_requested: Receiver<()>,
    is_open: Arc<AtomicBool>,
}

impl ParentHandle {
    pub fn new<'handle>() -> (Self, WindowHandle) {
        let (close_send, close_recv) = sync_channel(0);
        let is_open = Arc::new(AtomicBool::new(true));

        let handle = WindowHandle {
            raw_window_handle: None,
            close_requested: close_send,
            is_open: Arc::clone(&is_open),
        };

        (Self { close_requested: close_recv, is_open }, handle)
    }

    pub fn parent_did_drop(&self) -> bool {
        self.close_requested.try_recv().is_ok()
    }
}

impl Drop for ParentHandle {
    fn drop(&mut self) {
        self.is_open.store(false, Ordering::Relaxed);
    }
}

#[derive(Clone)]
pub(crate) struct WindowInner {
    // GlContext should be dropped **before** XcbConnection is dropped
    #[cfg(feature = "opengl")]
    gl_context: Option<Rc<GlContext>>,

    pub(crate) xcb_connection: Rc<XcbConnection>,
    window_id: XWindow,
    pub(crate) window_info: WindowInfo,
    visual_id: Visualid,
    mouse_cursor: Cell<MouseCursor>,

    pub(crate) close_requested: Cell<bool>,
}

#[derive(Clone)]
pub struct Window {
    pub(crate) inner: WindowInner,
}

// Hack to allow sending a RawWindowHandle between threads. Do not make public
struct SendableRwh(RawWindowHandle);

unsafe impl Send for SendableRwh {}

type WindowOpenResult = Result<SendableRwh, ()>;

impl Window {
    pub fn open_parented<P, H, B>(parent: &P, options: WindowOpenOptions, build: B) -> WindowHandle
    where
        P: HasWindowHandle,
        H: WindowHandler + 'static,
        B: FnOnce(crate::Window) -> H,
        B: Send + 'static,
    {
        // Convert parent into something that X understands
        let parent_handle =
            parent.window_handle().expect("parent window handle must be available").as_raw();
        let parent_id = match parent_handle {
            RawWindowHandle::Xlib(h) => h.window as u32,
            RawWindowHandle::Xcb(h) => h.window.get(),
            h => panic!("unsupported parent handle type {:?}", h),
        };

        let (tx, rx) = mpsc::sync_channel::<WindowOpenResult>(1);

        let (parent_handle, mut window_handle) = ParentHandle::new();

        thread::spawn(move || {
            Self::window_thread(Some(parent_id), options, build, tx.clone(), Some(parent_handle))
                .unwrap();
        });

        let SendableRwh(raw_window_handle) = rx.recv().unwrap().unwrap();
        window_handle.raw_window_handle = Some(raw_window_handle);

        window_handle
    }

    pub fn open_blocking<H, B>(options: WindowOpenOptions, build: B)
    where
        H: WindowHandler + 'static,
        B: FnOnce(crate::Window) -> H,
        B: Send + 'static,
    {
        let (tx, rx) = mpsc::sync_channel::<WindowOpenResult>(1);

        let thread = thread::spawn(move || {
            Self::window_thread(None, options, build, tx, None).unwrap();
        });

        let _ = rx.recv().unwrap().unwrap();

        thread.join().unwrap_or_else(|err| {
            eprintln!("Window thread panicked: {:#?}", err);
        });
    }

    fn window_thread<H, B>(
        parent: Option<u32>, options: WindowOpenOptions, build: B,
        tx: mpsc::SyncSender<WindowOpenResult>, parent_handle: Option<ParentHandle>,
    ) -> Result<(), Box<dyn Error>>
    where
        H: WindowHandler + 'static,
        B: FnOnce(crate::Window) -> H,
        B: Send + 'static,
    {
        // Connect to the X server
        // FIXME: baseview error type instead of unwrap()
        let xcb_connection = XcbConnection::new()?;

        // Get screen information
        let screen = xcb_connection.screen();
        let parent_id = parent.unwrap_or(screen.root);

        let gc_id = xcb_connection.conn.generate_id()?;
        xcb_connection.conn.create_gc(
            gc_id,
            parent_id,
            &CreateGCAux::new().foreground(screen.black_pixel).graphics_exposures(0),
        )?;

        let scaling = match options.scale {
            WindowScalePolicy::SystemScaleFactor => xcb_connection.get_scaling().unwrap_or(1.0),
            WindowScalePolicy::ScaleFactor(scale) => scale,
        };

        let window_info = WindowInfo::from_logical_size(options.size, scaling);

        #[cfg(feature = "opengl")]
        let visual_info =
            WindowVisualConfig::find_best_visual_config_for_gl(&xcb_connection, options.gl_config)?;

        #[cfg(not(feature = "opengl"))]
        let visual_info = WindowVisualConfig::find_best_visual_config(&xcb_connection)?;

        let window_id = xcb_connection.conn.generate_id()?;
        xcb_connection.conn.create_window(
            visual_info.visual_depth,
            window_id,
            parent_id,
            0,                                         // x coordinate of the new window
            0,                                         // y coordinate of the new window
            window_info.physical_size().width as u16,  // window width
            window_info.physical_size().height as u16, // window height
            0,                                         // window border
            WindowClass::INPUT_OUTPUT,
            visual_info.visual_id,
            &CreateWindowAux::new()
                .event_mask(
                    EventMask::EXPOSURE
                        | EventMask::POINTER_MOTION
                        | EventMask::BUTTON_PRESS
                        | EventMask::BUTTON_RELEASE
                        | EventMask::KEY_PRESS
                        | EventMask::KEY_RELEASE
                        | EventMask::STRUCTURE_NOTIFY
                        | EventMask::ENTER_WINDOW
                        | EventMask::LEAVE_WINDOW,
                )
                // As mentioned above, these two values are needed to be able to create a window
                // with a depth of 32-bits when the parent window has a different depth
                .colormap(visual_info.color_map)
                .border_pixel(0),
        )?;
        xcb_connection.conn.map_window(window_id)?;

        // Change window title
        let title = options.title;
        xcb_connection.conn.change_property8(
            PropMode::REPLACE,
            window_id,
            AtomEnum::WM_NAME,
            AtomEnum::STRING,
            title.as_bytes(),
        )?;

        xcb_connection.conn.change_property32(
            PropMode::REPLACE,
            window_id,
            xcb_connection.atoms.WM_PROTOCOLS,
            AtomEnum::ATOM,
            &[xcb_connection.atoms.WM_DELETE_WINDOW],
        )?;

        xcb_connection.conn.flush()?;

        // TODO: These APIs could use a couple tweaks now that everything is internal and there is
        //       no error handling anymore at this point. Everything is more or less unchanged
        //       compared to when raw-gl-context was a separate crate.
        #[cfg(feature = "opengl")]
        let gl_context = visual_info.fb_config.and_then(|fb_config| {
            use std::ffi::c_ulong;

            let window = window_id as c_ulong;
            let display = xcb_connection.dpy;

            // Because of the visual negotation we had to take some extra steps to create this context
            let context = unsafe { platform::GlContext::create(window, display, fb_config) };
            context.ok().map(|context| Rc::new(GlContext::new(context)))
        });

        let mut inner = WindowInner {
            xcb_connection: Rc::new(xcb_connection),
            window_id,
            window_info,
            visual_id: visual_info.visual_id,
            mouse_cursor: Cell::new(MouseCursor::default()),

            close_requested: Cell::new(false),

            #[cfg(feature = "opengl")]
            gl_context,
        };

        let window = crate::Window::new(Window { inner: inner.clone() });

        let mut handler = build(window.clone());

        // Send an initial window resized event so the user is alerted of
        // the correct dpi scaling.
        handler.on_event(window.clone(), Event::Window(WindowEvent::Resized(window_info)));

        let _ = tx.send(Ok(SendableRwh(
            window.window_handle().expect("this should be infallible!").as_raw(),
        )));

        EventLoop::new(inner, handler, parent_handle).run()?;

        Ok(())
    }

    pub fn set_mouse_cursor(&self, mouse_cursor: MouseCursor) {
        if self.inner.mouse_cursor.get() == mouse_cursor {
            return;
        }

        let xid = self.inner.xcb_connection.get_cursor(mouse_cursor).unwrap();

        if xid != 0 {
            let _ = self.inner.xcb_connection.conn.change_window_attributes(
                self.inner.window_id,
                &ChangeWindowAttributesAux::new().cursor(xid),
            );
            let _ = self.inner.xcb_connection.conn.flush();
        }

        self.inner.mouse_cursor.set(mouse_cursor);
    }

    pub fn set_mouse_position(&self, point: Point) {
        let point = point.to_physical(&self.inner.window_info);

        let _ = self.inner.xcb_connection.conn.warp_pointer(
            x11rb::NONE,
            self.inner.window_id,
            0,
            0,
            0,
            0,
            point.x as i16,
            point.y as i16,
        );
        let _ = self.inner.xcb_connection.conn.flush();
    }

    pub fn close(&mut self) {
        self.inner.close_requested.set(true);
    }

    pub fn has_focus(&mut self) -> bool {
        false
    }

    pub fn focus(&mut self) {}

    pub fn resize(&mut self, size: Size) {
        let scaling = self.inner.window_info.scale();
        let new_window_info = WindowInfo::from_logical_size(size, scaling);

        let _ = self.inner.xcb_connection.conn.configure_window(
            self.inner.window_id,
            &ConfigureWindowAux::new()
                .width(new_window_info.physical_size().width)
                .height(new_window_info.physical_size().height),
        );
        let _ = self.inner.xcb_connection.conn.flush();

        // This will trigger a `ConfigureNotify` event which will in turn change `self.window_info`
        // and notify the window handler about it
    }

    #[cfg(feature = "opengl")]
    pub fn gl_context(&self) -> Option<&crate::gl::GlContext> {
        self.inner.gl_context.as_ref().map(|gl| gl.as_ref())
    }
}

impl HasWindowHandle for Window {
    fn window_handle(&self) -> Result<raw_window_handle::WindowHandle<'_>, HandleError> {
        let mut handle = XlibWindowHandle::new(self.inner.window_id.into());

        handle.visual_id = self.inner.visual_id.into();

        // SAFETY: all fields are filled in, we should be good
        Ok(unsafe { raw_window_handle::WindowHandle::borrow_raw(RawWindowHandle::Xlib(handle)) })
    }
}

impl HasDisplayHandle for Window {
    fn display_handle(&self) -> Result<DisplayHandle<'_>, HandleError> {
        let display = self.inner.xcb_connection.dpy;
        let handle = XlibDisplayHandle::new(NonNull::new(display as *mut c_void), unsafe {
            x11::xlib::XDefaultScreen(display)
        });

        //handle.display = display as *mut c_void;
        //handle.screen = unsafe { x11::xlib::XDefaultScreen(display) };

        Ok(unsafe { DisplayHandle::borrow_raw(RawDisplayHandle::Xlib(handle)) })
    }
}

pub fn copy_to_clipboard(_data: &str) {
    todo!()
}
