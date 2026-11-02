//! Opens a new message with an attachment in the user's mail program. Windows: Simple MAPI
//! (Outlook, Thunderbird, eM Client, … register it); Linux: `xdg-email`. When neither takes the
//! attachment, a `mailto:` draft opens and the file is shown in the file manager so it can be
//! attached by hand.

use std::path::PathBuf;

pub struct Draft {
    /// may be empty – the user fills it in the mail program
    pub to: String,
    pub subject: String,
    pub body: String,
    pub attachment: PathBuf,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// a message window with the attachment is open
    Opened,
    /// the user closed the mail program's dialog (Simple MAPI only)
    #[cfg_attr(not(windows), allow(dead_code))]
    Cancelled,
    /// only a plain draft could be opened; the attachment has to be added by hand
    Manual,
}

/// Blocks until the mail program's dialog is open (Windows: until it is closed), so call it on
/// its own thread.
pub fn compose(d: &Draft) -> Outcome {
    match native(d) {
        Some(outcome) => outcome,
        None => {
            crate::platform::open_url(&mailto(d));
            crate::platform::reveal_file(&d.attachment);
            Outcome::Manual
        }
    }
}

/// `mailto:` link with subject and body (no attachment – the standard has none).
pub fn mailto(d: &Draft) -> String {
    format!("mailto:{}?subject={}&body={}", encode(d.to.trim()), encode(&d.subject), encode(&d.body))
}

/// Percent-encoding of everything except unreserved characters (RFC 3986), as UTF-8.
fn encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 3);
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
            // keep the address readable
            b'@' => out.push('@'),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

#[cfg(target_os = "linux")]
fn native(d: &Draft) -> Option<Outcome> {
    let mut cmd = std::process::Command::new("xdg-email");
    cmd.args(["--utf8", "--subject", &d.subject, "--body", &d.body, "--attach"]).arg(&d.attachment);
    if !d.to.trim().is_empty() {
        cmd.arg(d.to.trim());
    }
    match cmd.status() {
        Ok(s) if s.success() => Some(Outcome::Opened),
        Ok(s) => {
            tracing::warn!("xdg-email failed: {s}");
            None
        }
        Err(e) => {
            tracing::warn!("xdg-email not available: {e}");
            None
        }
    }
}

#[cfg(not(any(windows, target_os = "linux")))]
fn native(_d: &Draft) -> Option<Outcome> {
    None
}

#[cfg(windows)]
fn native(d: &Draft) -> Option<Outcome> {
    use std::ffi::c_void;
    use std::os::windows::ffi::OsStrExt;
    use std::ptr::null_mut;

    #[repr(C)]
    struct MapiRecipDescW {
        reserved: u32,
        recip_class: u32,
        name: *mut u16,
        address: *mut u16,
        eid_size: u32,
        entry_id: *mut c_void,
    }
    #[repr(C)]
    struct MapiFileDescW {
        reserved: u32,
        flags: u32,
        position: u32,
        path_name: *mut u16,
        file_name: *mut u16,
        file_type: *mut c_void,
    }
    #[repr(C)]
    struct MapiMessageW {
        reserved: u32,
        subject: *mut u16,
        note_text: *mut u16,
        message_type: *mut u16,
        date_received: *mut u16,
        conversation_id: *mut u16,
        flags: u32,
        originator: *mut MapiRecipDescW,
        recip_count: u32,
        recips: *mut MapiRecipDescW,
        file_count: u32,
        files: *mut MapiFileDescW,
    }
    type SendMailW = unsafe extern "system" fn(usize, usize, *mut MapiMessageW, u32, u32) -> u32;

    unsafe extern "system" {
        fn LoadLibraryW(name: *const u16) -> *mut c_void;
        fn GetProcAddress(module: *mut c_void, name: *const u8) -> *mut c_void;
        fn FreeLibrary(module: *mut c_void) -> i32;
    }
    #[link(name = "ole32")]
    unsafe extern "system" {
        fn CoInitializeEx(reserved: *mut c_void, coinit: u32) -> i32;
        fn CoUninitialize();
    }
    const MAPI_LOGON_UI: u32 = 0x1;
    const MAPI_DIALOG: u32 = 0x8;
    const MAPI_TO: u32 = 1;
    const SUCCESS_SUCCESS: u32 = 0;
    const MAPI_USER_ABORT: u32 = 1;
    const COINIT_APARTMENTTHREADED: u32 = 0x2;

    let wide = |s: &std::ffi::OsStr| s.encode_wide().chain(std::iter::once(0)).collect::<Vec<u16>>();
    let mut subject = wide(d.subject.as_ref());
    // MAPI expects CRLF line ends in the body
    let mut body = wide(d.body.replace("\r\n", "\n").replace('\n', "\r\n").as_ref());
    let mut path = wide(d.attachment.as_os_str());
    let mut file_name = wide(d.attachment.file_name().unwrap_or_default());
    let to = d.to.trim();
    let mut name = wide(to.as_ref());
    let mut address = wide(format!("SMTP:{to}").as_ref());

    let mut file = MapiFileDescW {
        reserved: 0,
        flags: 0,
        position: u32::MAX,
        path_name: path.as_mut_ptr(),
        file_name: file_name.as_mut_ptr(),
        file_type: null_mut(),
    };
    let mut recip = MapiRecipDescW {
        reserved: 0,
        recip_class: MAPI_TO,
        name: name.as_mut_ptr(),
        address: address.as_mut_ptr(),
        eid_size: 0,
        entry_id: null_mut(),
    };
    let mut msg = MapiMessageW {
        reserved: 0,
        subject: subject.as_mut_ptr(),
        note_text: body.as_mut_ptr(),
        message_type: null_mut(),
        date_received: null_mut(),
        conversation_id: null_mut(),
        flags: 0,
        originator: null_mut(),
        recip_count: if to.is_empty() { 0 } else { 1 },
        recips: if to.is_empty() { null_mut() } else { &mut recip },
        file_count: 1,
        files: &mut file,
    };

    let dll = wide("mapi32.dll".as_ref());
    // SAFETY: plain Win32 calls; every pointer in `msg` points into buffers that live until the
    // end of this function, and the library is only freed after the call returned.
    unsafe {
        let com = CoInitializeEx(null_mut(), COINIT_APARTMENTTHREADED);
        let module = LoadLibraryW(dll.as_ptr());
        let result = if module.is_null() {
            tracing::warn!("mapi32.dll could not be loaded");
            None
        } else {
            let proc = GetProcAddress(module, c"MAPISendMailW".as_ptr().cast());
            let r = if proc.is_null() {
                tracing::warn!("no MAPISendMailW – no mail program is registered for Simple MAPI");
                None
            } else {
                let send: SendMailW = std::mem::transmute(proc);
                match send(0, 0, &mut msg, MAPI_LOGON_UI | MAPI_DIALOG, 0) {
                    SUCCESS_SUCCESS => Some(Outcome::Opened),
                    MAPI_USER_ABORT => Some(Outcome::Cancelled),
                    code => {
                        tracing::warn!("MAPISendMailW failed with code {code}");
                        None
                    }
                }
            };
            FreeLibrary(module);
            r
        };
        // S_OK or S_FALSE: this call initialised COM and has to balance it
        if com >= 0 {
            CoUninitialize();
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mailto_is_percent_encoded() {
        let d = Draft {
            to: " doc@example.com ".into(),
            subject: "Glucose data 2026-10-01 – 2026-10-14".into(),
            body: "Hello & bye\n".into(),
            attachment: PathBuf::new(),
        };
        assert_eq!(
            mailto(&d),
            "mailto:doc@example.com?subject=Glucose%20data%202026-10-01%20%E2%80%93%202026-10-14&body=Hello%20%26%20bye%0A"
        );
    }
}
