use tauri::{App, AppHandle, Emitter, Manager, Runtime, WindowEvent};

const OPEN_FIND_EVENT: &str = "mothership-open-find";

pub fn configure_webview_platform<R: Runtime>(app: &App<R>) {
    #[cfg(windows)]
    configure_windows_webview(app);
}

pub fn configure_window_platform<R: Runtime>(app: &AppHandle<R>) {
    #[cfg(windows)]
    configure_windows_window(app);
}

#[cfg(windows)]
fn configure_windows_webview<R: Runtime>(app: &App<R>) {
    let Some(window) = app.get_webview_window("main") else {
        eprintln!("main webview window was not found while configuring WebView2");
        return;
    };

    let shortcut_window = window.clone();

    if let Err(error) = window.with_webview(|webview| {
        if let Err(error) = set_webview2_browser_accelerators(&webview, false) {
            eprintln!("failed to configure WebView2 browser accelerator keys: {error}");
        }

        if let Err(error) = bind_webview2_find_shortcut(&webview, shortcut_window) {
            eprintln!("failed to bind WebView2 find shortcut: {error}");
        }
    }) {
        eprintln!("failed to access WebView2 platform webview: {error}");
    }
}

#[cfg(windows)]
fn configure_windows_window<R: Runtime>(app: &AppHandle<R>) {
    let Some(window) = app.get_webview_window("main") else {
        eprintln!("main webview window was not found while configuring native window");
        return;
    };

    if let Err(error) = set_windows_window_icon(&window) {
        eprintln!("failed to set Windows window icon: {error}");
    }

    if let Err(error) = configure_windows_frameless_resize(&window) {
        eprintln!("failed to configure Windows frameless resize: {error}");
    }

    let resize_window = window.clone();
    window.on_window_event(move |event| {
        if matches!(
            event,
            WindowEvent::ScaleFactorChanged { .. } | WindowEvent::Focused(true)
        ) {
            if let Err(error) = configure_windows_frameless_resize(&resize_window) {
                eprintln!("failed to refresh Windows frameless resize: {error}");
            }
        }
    });
}

#[cfg(windows)]
fn set_windows_window_icon<R: Runtime>(
    window: &tauri::WebviewWindow<R>,
) -> Result<(), Box<dyn std::error::Error>> {
    let icon = tauri::image::Image::from_bytes(include_bytes!("../icons/icon.png"))?;
    window.set_icon(icon)?;
    Ok(())
}

#[cfg(windows)]
fn configure_windows_frameless_resize<R: Runtime>(
    window: &tauri::WebviewWindow<R>,
) -> Result<(), Box<dyn std::error::Error>> {
    enable_windows_native_resize_style(window)?;
    remove_tauri_resize_overlay(window)?;
    Ok(())
}

#[cfg(windows)]
fn enable_windows_native_resize_style<R: Runtime>(
    window: &tauri::WebviewWindow<R>,
) -> Result<(), Box<dyn std::error::Error>> {
    use windows::Win32::UI::WindowsAndMessaging::{
        GetWindowLongPtrW, SetWindowLongPtrW, SetWindowPos, GWL_STYLE, SWP_FRAMECHANGED,
        SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SWP_NOZORDER, WS_SIZEBOX,
    };

    let hwnd = window.hwnd()?;
    let style = unsafe { GetWindowLongPtrW(hwnd, GWL_STYLE) };
    let next_style = style | WS_SIZEBOX.0 as isize;

    if next_style != style {
        unsafe {
            SetWindowLongPtrW(hwnd, GWL_STYLE, next_style);
            SetWindowPos(
                hwnd,
                None,
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE | SWP_FRAMECHANGED,
            )?;
        }
    }

    Ok(())
}

#[cfg(windows)]
fn remove_tauri_resize_overlay<R: Runtime>(
    window: &tauri::WebviewWindow<R>,
) -> Result<(), Box<dyn std::error::Error>> {
    use windows::{
        core::w,
        Win32::UI::WindowsAndMessaging::{DestroyWindow, FindWindowExW},
    };

    let hwnd = window.hwnd()?;

    if let Ok(overlay) = unsafe {
        FindWindowExW(
            Some(hwnd),
            None,
            w!("TAURI_DRAG_RESIZE_BORDERS"),
            w!("TAURI_DRAG_RESIZE_WINDOW"),
        )
    } {
        unsafe {
            DestroyWindow(overlay)?;
        }
    }

    Ok(())
}

#[cfg(windows)]
fn set_webview2_browser_accelerators(
    webview: &tauri::webview::PlatformWebview,
    enabled: bool,
) -> windows::core::Result<()> {
    use webview2_com::Microsoft::Web::WebView2::Win32::ICoreWebView2Settings3;
    use windows::core::Interface;

    unsafe {
        let core_webview = webview.controller().CoreWebView2()?;
        let settings = core_webview.Settings()?;
        let settings3 = settings.cast::<ICoreWebView2Settings3>()?;
        settings3.SetAreBrowserAcceleratorKeysEnabled(enabled)?;
    }

    Ok(())
}

#[cfg(windows)]
fn bind_webview2_find_shortcut<R: Runtime>(
    webview: &tauri::webview::PlatformWebview,
    window: tauri::WebviewWindow<R>,
) -> windows::core::Result<()> {
    use webview2_com::{
        AcceleratorKeyPressedEventHandler,
        Microsoft::Web::WebView2::Win32::COREWEBVIEW2_KEY_EVENT_KIND,
    };

    let handler = AcceleratorKeyPressedEventHandler::create(Box::new(move |_sender, args| {
        let Some(args) = args else {
            return Ok(());
        };

        let mut key_kind = COREWEBVIEW2_KEY_EVENT_KIND::default();
        let mut virtual_key = 0;

        unsafe {
            args.KeyEventKind(&mut key_kind)?;
            args.VirtualKey(&mut virtual_key)?;
        }

        if is_find_shortcut(key_kind, virtual_key) {
            unsafe {
                args.SetHandled(true)?;
            }

            let window = window.clone();
            std::thread::spawn(move || {
                let _ = window.emit(OPEN_FIND_EVENT, ());
            });
        }

        Ok(())
    }));

    let mut token = 0;

    unsafe {
        webview
            .controller()
            .add_AcceleratorKeyPressed(&handler, &mut token)?;
    }

    Ok(())
}

#[cfg(windows)]
fn is_find_shortcut(
    kind: webview2_com::Microsoft::Web::WebView2::Win32::COREWEBVIEW2_KEY_EVENT_KIND,
    virtual_key: u32,
) -> bool {
    use webview2_com::Microsoft::Web::WebView2::Win32::{
        COREWEBVIEW2_KEY_EVENT_KIND_KEY_DOWN, COREWEBVIEW2_KEY_EVENT_KIND_SYSTEM_KEY_DOWN,
    };
    use windows::Win32::UI::Input::KeyboardAndMouse::{GetKeyState, VK_CONTROL};

    const F_KEY: u32 = 0x46;
    const HIGH_ORDER_KEY_DOWN_BIT: i32 = 0x8000;

    if virtual_key != F_KEY
        || (kind != COREWEBVIEW2_KEY_EVENT_KIND_KEY_DOWN
            && kind != COREWEBVIEW2_KEY_EVENT_KIND_SYSTEM_KEY_DOWN)
    {
        return false;
    }

    let control_state = unsafe { GetKeyState(VK_CONTROL.0.into()) };
    (i32::from(control_state) & HIGH_ORDER_KEY_DOWN_BIT) != 0
}
