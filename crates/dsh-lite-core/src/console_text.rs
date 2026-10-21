//! Decoding bytes that a child process wrote to a pipe or a file.
//!
//! Node-based tools write UTF-8, but anything that goes through `cmd.exe`
//! (a missing command, a `.cmd` shim failure, `chcp` warnings) writes in the
//! process' OEM code page. On a Simplified-Chinese Windows that is 936, so
//! decoding such output as UTF-8 produces mojibake — and `read_to_string`
//! fails outright, which silently turns the whole capture into an empty string.
//!
//! The rule here is therefore: **UTF-8 first, OEM code page second, lossy last.**
//! A shell cannot make `cmd.exe` emit UTF-8 (verified: `chcp 65001` does not
//! affect redirected output), so the reader has to be tolerant instead.

/// Decodes bytes from a child process, preferring UTF-8.
pub fn decode(bytes: &[u8]) -> String {
    if let Ok(text) = std::str::from_utf8(bytes) {
        return text.to_owned();
    }

    #[cfg(windows)]
    if let Some(text) = decode_code_page(bytes, OEM_CODE_PAGE) {
        return text;
    }

    String::from_utf8_lossy(bytes).into_owned()
}

/// The console code page Windows uses for redirected `cmd.exe` output.
#[cfg(windows)]
const OEM_CODE_PAGE: u32 = 1;

/// Decodes bytes in an explicit Windows code page. `None` when the conversion
/// fails (unknown code page, embedded NUL handled leniently).
#[cfg(windows)]
pub fn decode_code_page(bytes: &[u8], code_page: u32) -> Option<String> {
    if bytes.is_empty() {
        return Some(String::new());
    }

    let length = i32::try_from(bytes.len()).ok()?;
    let needed = unsafe {
        MultiByteToWideChar(
            code_page,
            0,
            bytes.as_ptr(),
            length,
            std::ptr::null_mut(),
            0,
        )
    };
    if needed <= 0 {
        return None;
    }

    let mut wide = vec![0u16; needed as usize];
    let written = unsafe {
        MultiByteToWideChar(
            code_page,
            0,
            bytes.as_ptr(),
            length,
            wide.as_mut_ptr(),
            needed,
        )
    };
    if written <= 0 {
        return None;
    }
    wide.truncate(written as usize);
    Some(String::from_utf16_lossy(&wide))
}

#[cfg(windows)]
unsafe extern "system" {
    fn MultiByteToWideChar(
        code_page: u32,
        flags: u32,
        multi_byte_str: *const u8,
        multi_byte_count: i32,
        wide_char_str: *mut u16,
        wide_char_count: i32,
    ) -> i32;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utf8_passes_through_unchanged() {
        assert_eq!(decode("启动失败 npm 报错".as_bytes()), "启动失败 npm 报错");
        assert_eq!(decode(b""), "");
    }

    #[cfg(windows)]
    #[test]
    fn oem_bytes_decode_with_the_requested_code_page() {
        // "不是" in code page 936 (GBK).
        let gbk = [0xB2u8, 0xBB, 0xCA, 0xC7];
        assert_eq!(decode_code_page(&gbk, 936).as_deref(), Some("不是"));
        assert_eq!(decode(&gbk), decode_code_page(&gbk, 1).unwrap_or_default());
    }

    #[cfg(windows)]
    #[test]
    fn invalid_bytes_never_panic() {
        // A lone continuation byte: unreadable in UTF-8 and in GBK.
        let junk = [0x80u8, 0xFF, 0x00];
        let decoded = decode(&junk);
        assert!(!decoded.is_empty());
    }

    #[cfg(windows)]
    #[test]
    fn a_failing_cmd_error_decodes_without_replacement_characters() {
        // The case this module exists for: `cmd.exe` reports a missing command
        // in the OEM code page, and the shell used to show it as mojibake.
        let output = std::process::Command::new("cmd.exe")
            .args(["/c", "dsh-desktop-no-such-command-xyz"])
            .output()
            .expect("cmd.exe runs");
        let text = decode(&output.stderr);
        assert!(!text.is_empty(), "cmd printed nothing to decode");
        assert!(
            !text.contains('\u{FFFD}'),
            "decoding produced replacement characters: {text}"
        );
        assert!(
            text.contains("dsh-desktop-no-such-command-xyz"),
            "the decoded text lost the command name: {text}"
        );
    }
}
