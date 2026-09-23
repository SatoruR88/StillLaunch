//! WM_COPYDATA IPC between the `--profile` frontend and the resident.
//!
//! Envelope: `COPYDATASTRUCT.dwData` carries a magic + protocol version, and
//! `lpData` carries a NUL-terminated UTF-16 JSON document. JSON keeps the
//! protocol extensible (config reload, quit) without new framing and reuses
//! serde_json, which is already a dependency.
//!
//! WM_COPYDATA semantics this design relies on:
//! - the sender MUST use a synchronous send (SendMessageTimeoutW, bounded
//!   so a hung resident cannot block the caller forever): `lpData` is only
//!   valid until the call returns, so PostMessage would point at freed
//!   memory;
//! - the receiver MUST copy `lpData` before returning from the window proc;
//! - UIPI can silently block delivery across integrity levels (e.g. an
//!   elevated resident cannot be reached from a non-elevated caller).

use serde::{Deserialize, Serialize};
use std::fmt;
use std::iter::once;

/// Protocol identifier + version packed into `COPYDATASTRUCT.dwData`:
/// `0x504C` ("PL") in the high bits, protocol version 1 in the low bits.
/// Receivers must reject anything else so a foreign WM_COPYDATA broadcast is
/// never interpreted as a request.
pub const IPC_MAGIC: usize = 0x504C_0001;

/// Hard cap on the payload size. A run_profile request is a GUID inside
/// ~100 bytes of JSON; anything larger is malformed or hostile and rejected
/// before it can force a big allocation.
pub const MAX_IPC_BYTES: u32 = 8 * 1024;

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum IpcRequest {
    /// Run a profile, addressed by its stable GUID.
    RunProfile { profile: String },
}

#[derive(Debug)]
pub enum IpcError {
    /// `dwData` did not match the protocol magic/version.
    BadMagic(usize),
    /// `lpData` was null, `cbData` was zero, or the payload had no content.
    MalformedPayload,
    /// The payload exceeded [`MAX_IPC_BYTES`].
    Oversized(u64),
    /// The payload was not valid UTF-16.
    NotUtf16,
    /// The payload was not valid protocol JSON.
    BadJson(String),
}

impl fmt::Display for IpcError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            IpcError::BadMagic(v) => write!(f, "unexpected IPC magic {v:#x}"),
            IpcError::MalformedPayload => write!(f, "empty or malformed IPC payload"),
            IpcError::Oversized(n) => {
                write!(
                    f,
                    "IPC payload of {n} bytes exceeds the {MAX_IPC_BYTES} byte limit"
                )
            }
            IpcError::NotUtf16 => write!(f, "IPC payload is not valid UTF-16"),
            IpcError::BadJson(e) => write!(f, "IPC payload is not valid protocol JSON: {e}"),
        }
    }
}

impl std::error::Error for IpcError {}

/// Serializes a request to a NUL-terminated UTF-16 payload.
pub fn encode(request: &IpcRequest) -> Result<Vec<u16>, IpcError> {
    let json = serde_json::to_string(request).map_err(|e| IpcError::BadJson(e.to_string()))?;
    Ok(json.encode_utf16().chain(once(0)).collect())
}

/// Validates the envelope and decodes the payload. `data` is the UTF-16
/// content of `lpData` (`cbData / 2` units), which the receiver must have
/// already copied out of the COPYDATASTRUCT. A trailing NUL is stripped if
/// present but not required.
pub fn decode(dw_data: usize, data: &[u16]) -> Result<IpcRequest, IpcError> {
    if dw_data != IPC_MAGIC {
        return Err(IpcError::BadMagic(dw_data));
    }
    let byte_len = data.len() as u64 * 2;
    if byte_len == 0 {
        return Err(IpcError::MalformedPayload);
    }
    if byte_len > MAX_IPC_BYTES as u64 {
        return Err(IpcError::Oversized(byte_len));
    }
    let body = match data.last() {
        Some(&0) => &data[..data.len() - 1],
        _ => data,
    };
    let text = String::from_utf16(body).map_err(|_| IpcError::NotUtf16)?;
    serde_json::from_str(&text).map_err(|e| IpcError::BadJson(e.to_string()))
}

/// What happened when trying to hand a request to the resident.
#[cfg(windows)]
#[derive(Debug, PartialEq, Eq)]
pub enum ForwardOutcome {
    /// The resident accepted the request and started a worker.
    Delivered,
    /// No resident window exists; the caller should run the profile itself.
    NoResident,
    /// A resident exists but the send did not deliver: the request was
    /// rejected (unknown profile id, malformed payload, saturated workers),
    /// blocked by UIPI, or the resident failed to answer in time.
    Rejected,
}

/// Finds the resident's message-only window. Returns null when no resident
/// is running.
#[cfg(windows)]
pub fn find_resident_window() -> windows_sys::Win32::Foundation::HWND {
    use std::ptr;
    use windows_sys::Win32::UI::WindowsAndMessaging::{FindWindowExW, HWND_MESSAGE};

    let class = to_wide(crate::resident::WINDOW_CLASS);
    // SAFETY: `class` is a NUL-terminated buffer that outlives the call.
    // HWND_MESSAGE as the parent is what makes message-only windows visible
    // to the search.
    unsafe { FindWindowExW(HWND_MESSAGE, ptr::null_mut(), class.as_ptr(), ptr::null()) }
}

/// How long `forward_run_profile` waits for the resident's window proc to
/// answer before giving up. WM_COPYDATA must be sent (not posted), but a
/// stuck resident must not block the caller indefinitely.
#[cfg(windows)]
const FORWARD_TIMEOUT_MS: u32 = 3000;

/// Sends a run_profile request to the resident. Fire-and-forget: success
/// only means the resident accepted the request, not that the profile ran
/// to completion.
#[cfg(windows)]
pub fn forward_run_profile(profile_id: &str) -> Result<ForwardOutcome, IpcError> {
    use windows_sys::Win32::System::DataExchange::COPYDATASTRUCT;
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        SMTO_ABORTIFHUNG, SMTO_NORMAL, SendMessageTimeoutW, WM_COPYDATA,
    };

    let payload = encode(&IpcRequest::RunProfile {
        profile: profile_id.to_string(),
    })?;
    let hwnd = find_resident_window();
    if hwnd.is_null() {
        return Ok(ForwardOutcome::NoResident);
    }
    // SAFETY: SendMessageTimeoutW is synchronous like SendMessageW, so
    // `payload` stays valid for the receiver to copy while the call runs;
    // cbData is the byte length of the buffer lpData points to, and
    // `result` is a valid out-parameter for the window proc's return value.
    // SMTO_ABORTIFHUNG bounds the wait: if the resident is hung the call
    // returns 0 instead of blocking this process forever.
    let mut result: usize = 0;
    let delivered = unsafe {
        let cds = COPYDATASTRUCT {
            dwData: IPC_MAGIC,
            cbData: (payload.len() * 2) as u32,
            lpData: payload.as_ptr() as *mut core::ffi::c_void,
        };
        SendMessageTimeoutW(
            hwnd,
            WM_COPYDATA,
            0,
            &cds as *const _ as isize,
            SMTO_ABORTIFHUNG | SMTO_NORMAL,
            FORWARD_TIMEOUT_MS,
            &mut result,
        )
    };
    // A 0 return is a timeout or a failed send; a 0 result is the window
    // proc rejecting the request. Both surface as Rejected to the caller.
    Ok(if delivered != 0 && result != 0 {
        ForwardOutcome::Delivered
    } else {
        ForwardOutcome::Rejected
    })
}

#[cfg(windows)]
fn to_wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(once(0)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decode_encoded(request: &IpcRequest) -> Result<IpcRequest, IpcError> {
        let payload = encode(request).unwrap();
        decode(IPC_MAGIC, &payload)
    }

    #[test]
    fn run_profile_roundtrip() {
        let req = decode_encoded(&IpcRequest::RunProfile {
            profile: "{11111111-2222-3333-4444-555555555555}".to_string(),
        })
        .unwrap();
        match req {
            IpcRequest::RunProfile { profile } => {
                assert_eq!(profile, "{11111111-2222-3333-4444-555555555555}");
            }
        }
    }

    #[test]
    fn rejects_wrong_magic() {
        let payload = encode(&IpcRequest::RunProfile {
            profile: "x".to_string(),
        })
        .unwrap();
        let err = decode(0xDEAD, &payload).unwrap_err();
        assert!(matches!(err, IpcError::BadMagic(0xDEAD)));
    }

    #[test]
    fn rejects_empty_payload() {
        let err = decode(IPC_MAGIC, &[]).unwrap_err();
        assert!(matches!(err, IpcError::MalformedPayload));
    }

    #[test]
    fn rejects_oversized_payload() {
        let big = vec![0u16; (MAX_IPC_BYTES as usize / 2) + 1];
        let err = decode(IPC_MAGIC, &big).unwrap_err();
        assert!(matches!(err, IpcError::Oversized(_)));
    }

    #[test]
    fn rejects_invalid_utf16() {
        // Lone surrogate: not representable as UTF-16 text.
        let err = decode(IPC_MAGIC, &[0xD800, 0]).unwrap_err();
        assert!(matches!(err, IpcError::NotUtf16));
    }

    #[test]
    fn rejects_bad_json_and_unknown_fields() {
        let not_json = to_wide_lossy("{ not json");
        assert!(matches!(
            decode(IPC_MAGIC, &not_json).unwrap_err(),
            IpcError::BadJson(_)
        ));

        let extra = to_wide_lossy(r#"{"type":"run_profile","profile":"x","extra":1}"#);
        assert!(matches!(
            decode(IPC_MAGIC, &extra).unwrap_err(),
            IpcError::BadJson(_)
        ));

        let unknown = to_wide_lossy(r#"{"type":"quit"}"#);
        assert!(matches!(
            decode(IPC_MAGIC, &unknown).unwrap_err(),
            IpcError::BadJson(_)
        ));
    }

    /// A sender that omits the trailing NUL still decodes.
    #[test]
    fn accepts_payload_without_trailing_nul() {
        let payload = encode(&IpcRequest::RunProfile {
            profile: "x".to_string(),
        })
        .unwrap();
        let stripped = &payload[..payload.len() - 1];
        assert!(decode(IPC_MAGIC, stripped).is_ok());
    }

    fn to_wide_lossy(s: &str) -> Vec<u16> {
        s.encode_utf16().collect()
    }
}
