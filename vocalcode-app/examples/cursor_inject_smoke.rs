//! Proves the delivery half of cursor-native dictation.
//!
//! The gesture tests show a double-click reaches `HandsFreeStart`/`ForceStop`.
//! They cannot show that words end up in the field, because that happens inside
//! the engine's injector. This probe closes that gap directly: it creates a real
//! Win32 `EDIT` control, focuses it, and runs the *same* `EnigoInjector` the
//! engine uses after a dictation, then reads the control's text back.
//!
//! It is the closest thing to "dictate into a field" that can run without a
//! microphone: every step after the transcript — focus token, editable-target
//! check, typing, caret position — is the production path.
//!
//! ```text
//! cargo run -p vocalcode-app --features community --example cursor_inject_smoke
//! ```

#[cfg(windows)]
fn main() -> anyhow::Result<()> {
    use vocalcode_core::traits::TextInjector;

    use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetWindowTextW,
        PeekMessageW, RegisterClassW, SetForegroundWindow, ShowWindow, TranslateMessage, MSG,
        PM_REMOVE, SW_SHOW, WNDCLASSW, WS_BORDER, WS_CHILD, WS_EX_CLIENTEDGE, WS_OVERLAPPEDWINDOW,
        WS_VISIBLE,
    };

    // Declared here because SetFocus lives behind the keyboard-and-mouse
    // feature, which this crate does not enable.
    #[link(name = "user32")]
    unsafe extern "system" {
        fn SetFocus(hwnd: *mut core::ffi::c_void) -> *mut core::ffi::c_void;
        fn keybd_event(vk: u8, scan: u8, flags: u32, extra: usize);
        fn GetForegroundWindow() -> *mut core::ffi::c_void;
    }

    /// Take the foreground the way a user would.
    ///
    /// Windows refuses `SetForegroundWindow` from a process that is not already
    /// foreground, so a freshly launched probe is locked out. A synthetic ALT
    /// press is the documented way to release that lock; without it the injector
    /// correctly reports "no text field was focused".
    unsafe fn force_foreground(window: *mut core::ffi::c_void) -> bool {
        const VK_MENU: u8 = 0x12;
        const KEYEVENTF_KEYUP: u32 = 0x0002;
        for _ in 0..40 {
            keybd_event(VK_MENU, 0, 0, 0);
            keybd_event(VK_MENU, 0, KEYEVENTF_KEYUP, 0);
            SetForegroundWindow(window);
            if GetForegroundWindow() == window {
                return true;
            }
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
        false
    }

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    let phrase = "Hello from the cursor.";

    let text = unsafe {
        let instance = GetModuleHandleW(std::ptr::null());
        anyhow::ensure!(!instance.is_null(), "GetModuleHandleW failed");

        let class_name = wide("VocalCodeInjectSmoke");
        let window_title = wide("VocalCode injection probe");
        let edit_class = wide("EDIT");
        let empty = wide("");

        let class = WNDCLASSW {
            style: 0,
            lpfnWndProc: Some(DefWindowProcW),
            cbClsExtra: 0,
            cbWndExtra: 0,
            hInstance: instance,
            hIcon: std::ptr::null_mut(),
            hCursor: std::ptr::null_mut(),
            hbrBackground: std::ptr::null_mut(),
            lpszMenuName: std::ptr::null(),
            lpszClassName: class_name.as_ptr(),
        };
        anyhow::ensure!(
            RegisterClassW(&class) != 0,
            "RegisterClassW failed: {}",
            std::io::Error::last_os_error()
        );

        let window = CreateWindowExW(
            0,
            class_name.as_ptr(),
            window_title.as_ptr(),
            WS_OVERLAPPEDWINDOW | WS_VISIBLE,
            160,
            160,
            520,
            220,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            instance,
            std::ptr::null_mut(),
        );
        anyhow::ensure!(
            !window.is_null(),
            "CreateWindowExW failed: {}",
            std::io::Error::last_os_error()
        );

        // A real edit control: the injector has to identify it through UI
        // Automation exactly as it would identify Notepad's.
        let edit = CreateWindowExW(
            WS_EX_CLIENTEDGE,
            edit_class.as_ptr(),
            empty.as_ptr(),
            WS_CHILD | WS_VISIBLE | WS_BORDER,
            12,
            12,
            480,
            160,
            window,
            std::ptr::null_mut(),
            instance,
            std::ptr::null_mut(),
        );
        anyhow::ensure!(
            !edit.is_null(),
            "could not create the edit control: {}",
            std::io::Error::last_os_error()
        );

        ShowWindow(window, SW_SHOW);
        let focused = force_foreground(window);
        SetFocus(edit);
        anyhow::ensure!(
            focused,
            "the probe could not take the foreground, so no field could be focused"
        );

        // Let the window manager settle before the injector looks for focus.
        let mut message: MSG = std::mem::zeroed();
        for _ in 0..40 {
            while PeekMessageW(&mut message, std::ptr::null_mut(), 0, 0, PM_REMOVE) != 0 {
                TranslateMessage(&message);
                DispatchMessageW(&message);
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }

        let injector = vocalcode_platform::EnigoInjector::new(false);
        injector.begin_utterance()?;
        let delivered = injector.inject_text(phrase);
        injector.end_utterance();
        delivered?;

        // Typing is asynchronous; give the control a moment to consume it.
        for _ in 0..30 {
            while PeekMessageW(&mut message, std::ptr::null_mut(), 0, 0, PM_REMOVE) != 0 {
                TranslateMessage(&message);
                DispatchMessageW(&message);
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }

        let mut buffer = [0u16; 256];
        let length = GetWindowTextW(edit, buffer.as_mut_ptr(), buffer.len() as i32);
        let text = String::from_utf16_lossy(&buffer[..length.max(0) as usize]);

        DestroyWindow(window);
        text
    };

    println!("field now contains: {text:?}");
    anyhow::ensure!(
        text.contains(phrase),
        "the injector did not deliver text into the focused edit control"
    );
    println!("RESULT=OK the words landed in the focused field at its caret");
    Ok(())
}

#[cfg(not(windows))]
fn main() {
    println!("cursor_inject_smoke is a Windows-only probe");
}
