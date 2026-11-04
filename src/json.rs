//! Minimal JSON emission for the ncdu export format.

use std::io::Write;

/// Append `s` as a quoted, escaped JSON string.
pub fn push_str(out: &mut Vec<u8>, s: &str) {
    out.push(b'"');
    let bytes = s.as_bytes();
    let mut start = 0;
    for (i, &b) in bytes.iter().enumerate() {
        let esc: &[u8] = match b {
            b'"' => b"\\\"",
            b'\\' => b"\\\\",
            b'\n' => b"\\n",
            b'\r' => b"\\r",
            b'\t' => b"\\t",
            0x00..=0x1f | 0x7f => {
                out.extend_from_slice(&bytes[start..i]);
                let _ = write!(out, "\\u{b:04x}");
                start = i + 1;
                continue;
            }
            _ => continue,
        };
        out.extend_from_slice(&bytes[start..i]);
        out.extend_from_slice(esc);
        start = i + 1;
    }
    out.extend_from_slice(&bytes[start..]);
    out.push(b'"');
}

/// Append a quoted JSON string from raw OS bytes (invalid UTF-8 becomes U+FFFD).
pub fn push_name(out: &mut Vec<u8>, name: &[u8]) {
    push_str(out, &String::from_utf8_lossy(name));
}

/// Append `,{...}` describing one regular file.
pub fn push_file(out: &mut Vec<u8>, name: &[u8], asize: u64, dsize: u64, ino: u64, nlink: u64) {
    out.extend_from_slice(b",{\"name\":");
    push_name(out, name);
    let _ = write!(out, ",\"asize\":{asize},\"dsize\":{dsize},\"ino\":{ino}");
    if nlink > 1 {
        let _ = write!(out, ",\"hlnkc\":true,\"nlink\":{nlink}");
    }
    out.push(b'}');
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(x: &str) -> String {
        let mut v = Vec::new();
        push_str(&mut v, x);
        String::from_utf8(v).unwrap()
    }

    #[test]
    fn escapes() {
        assert_eq!(s("plain"), "\"plain\"");
        assert_eq!(s("a\"b\\c"), r#""a\"b\\c""#);
        assert_eq!(s("a\nb\tc"), r#""a\nb\tc""#);
        assert_eq!(s("\u{1}x\u{7f}"), r#""\u0001x\u007f""#);
        assert_eq!(s("héllo ✓"), "\"héllo ✓\"");
    }

    #[test]
    fn lossy_names() {
        let mut v = Vec::new();
        push_name(&mut v, b"a\xffb");
        assert_eq!(String::from_utf8(v).unwrap(), "\"a\u{fffd}b\"");
    }

    #[test]
    fn file_entry() {
        let mut v = Vec::new();
        push_file(&mut v, b"f", 10, 4096, 7, 1);
        push_file(&mut v, b"g", 0, 0, 8, 3);
        assert_eq!(
            String::from_utf8(v).unwrap(),
            r#",{"name":"f","asize":10,"dsize":4096,"ino":7},{"name":"g","asize":0,"dsize":0,"ino":8,"hlnkc":true,"nlink":3}"#
        );
    }
}
