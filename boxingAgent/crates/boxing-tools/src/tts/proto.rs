//! Volcano TTS 二进制帧编解码 — 对等 demo protocols.py 的 `Message`。
//!
//! 帧布局（大端）：4 字节头 [ver|hsz, type|flag, ser|comp, pad...] + 条件体。
//! 我们只 marshal `FullClientRequest`+`NoSeq`；unmarshal 服务端帧。
//! 详见 spec §6。

use crate::ToolError;

/// 解析后的服务端帧分类（客户端只关心这几种）。
#[derive(Debug)]
pub enum Frame {
    /// AudioOnlyServer (type 0xB) — payload 是一段音频。
    Audio(Vec<u8>),
    /// FullServerResponse (type 0x9) + WithEvent + event=SessionFinished(152)。
    SessionFinished,
    /// Error (type 0xF) — error_code + payload(通常是 JSON 错误描述)。
    Error { code: u32, payload: Vec<u8> },
    /// 其它帧（忽略）。
    Other,
}

/// Marshal a FullClientRequest + NoSeq + JSON + None frame around `payload`.
///
/// 头: 0x11 0x10 0x10 0x00  体: [u32 BE len][payload]
pub fn marshal_request(payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(8 + payload.len());
    out.extend_from_slice(&[0x11, 0x10, 0x10, 0x00]);
    out.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    out.extend_from_slice(payload);
    out
}

/// Unmarshal a server frame into a [`Frame`] classification.
pub fn unmarshal(data: &[u8]) -> Result<Frame, ToolError> {
    if data.len() < 4 {
        return Err(ToolError::Other("volcano 帧不足 4 字节".into()));
    }
    let header_bytes = 4 * (data[0] & 0x0F) as usize;
    let type_and_flag = data[1];
    let msg_type = type_and_flag >> 4;
    let flag = type_and_flag & 0x0F;
    let mut i = header_bytes;
    if i > data.len() {
        return Err(ToolError::Other("volcano 帧头越界".into()));
    }

    // reader 规则（对等 protocols.py::_get_readers）
    let want_seq = matches!(msg_type, 0x1..=0x2 | 0x9 | 0xB | 0xC)
        && matches!(flag, 0x1 | 0x3); // PositiveSeq | NegativeSeq
    let is_error = msg_type == 0xF;

    let mut error_code = 0u32;
    if want_seq {
        read_i32_be(data, &mut i)?; // sequence（不用）
    } else if is_error {
        error_code = read_u32_be(data, &mut i)?;
    }

    let mut event = 0i32;
    if flag == 0x4 {
        // WithEvent
        event = read_i32_be(data, &mut i)?;
        // session_id：event ∉ {1,2,50,51,52} 时读
        if !matches!(event, 1..=2 | 50..=52) {
            let _sid = read_len_prefixed(data, &mut i)?;
        }
        // connect_id：event ∈ {50,51,52} 时读
        if matches!(event, 50..=52) {
            let _cid = read_len_prefixed(data, &mut i)?;
        }
    }
    let payload = read_len_prefixed(data, &mut i)?;

    Ok(match msg_type {
        0xB => Frame::Audio(payload),
        0x9 if flag == 0x4 && event == 152 => Frame::SessionFinished,
        0xF => Frame::Error { code: error_code, payload },
        _ => Frame::Other,
    })
}

fn read_u32_be(data: &[u8], i: &mut usize) -> Result<u32, ToolError> {
    read_fix::<4>(data, i).map(u32::from_be_bytes)
}

fn read_i32_be(data: &[u8], i: &mut usize) -> Result<i32, ToolError> {
    read_fix::<4>(data, i).map(i32::from_be_bytes)
}

fn read_fix<const N: usize>(data: &[u8], i: &mut usize) -> Result<[u8; N], ToolError> {
    if *i + N > data.len() {
        return Err(ToolError::Other("volcano 帧体截断".into()));
    }
    let mut buf = [0u8; N];
    buf.copy_from_slice(&data[*i..*i + N]);
    *i += N;
    Ok(buf)
}

fn read_len_prefixed(data: &[u8], i: &mut usize) -> Result<Vec<u8>, ToolError> {
    let len = read_u32_be(data, i)? as usize;
    if *i + len > data.len() {
        return Err(ToolError::Other("volcano 帧长度越界".into()));
    }
    let v = data[*i..*i + len].to_vec();
    *i += len;
    Ok(v)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn marshal_request_exact_bytes() {
        let out = marshal_request(b"hello");
        // 头 0x11 0x10 0x10 0x00 + u32 BE len(5) + "hello"
        assert_eq!(
            out,
            vec![0x11, 0x10, 0x10, 0x00, 0, 0, 0, 5, b'h', b'e', b'l', b'l', b'o']
        );
    }

    #[test]
    fn unmarshal_audio_only_no_seq() {
        // type=AudioOnlyServer(0xB)|flag=NoSeq(0) -> byte1 = 0xB0
        let f = vec![0x11, 0xB0, 0x10, 0x00, 0, 0, 0, 3, b'a', b'b', b'c'];
        match unmarshal(&f).unwrap() {
            Frame::Audio(p) => assert_eq!(p, b"abc"),
            other => panic!("expected Audio, got {other:?}"),
        }
    }

    #[test]
    fn unmarshal_audio_only_with_sequence() {
        // type=0xB|flag=PositiveSeq(1) -> byte1 = 0xB1；先读 i32 sequence(=7) 再 payload
        let f = vec![0x11, 0xB1, 0x10, 0x00, 0, 0, 0, 7, 0, 0, 0, 3, b'x', b'y', b'z'];
        match unmarshal(&f).unwrap() {
            Frame::Audio(p) => assert_eq!(p, b"xyz"),
            other => panic!("expected Audio, got {other:?}"),
        }
    }

    #[test]
    fn unmarshal_session_finished() {
        // type=FullServerResponse(0x9)|flag=WithEvent(4) -> byte1 = 0x94
        // event i32=152(0x98)；session_id len=0；connect_id 不读(152∉{50,51,52})；payload len=0
        let f = vec![
            0x11, 0x94, 0x10, 0x00, // 头
            0x00, 0x00, 0x00, 0x98, // event = 152
            0x00, 0x00, 0x00, 0x00, // session_id len = 0
            0x00, 0x00, 0x00, 0x00, // payload len = 0
        ];
        assert!(matches!(unmarshal(&f).unwrap(), Frame::SessionFinished));
    }

    #[test]
    fn unmarshal_error() {
        // type=Error(0xF)|flag=NoSeq(0) -> byte1 = 0xF0；error_code u32=42；payload "bad"
        let f = vec![
            0x11, 0xF0, 0x10, 0x00, 0x00, 0x00, 0x00, 42, 0x00, 0x00, 0x00, 3, b'b', b'a', b'd',
        ];
        match unmarshal(&f).unwrap() {
            Frame::Error { code, payload } => {
                assert_eq!(code, 42);
                assert_eq!(payload, b"bad");
            }
            other => panic!("expected Error, got {other:?}"),
        }
    }

    #[test]
    fn unmarshal_other_frame_type_is_other() {
        // 未知 type（如 0x2 AudioOnlyClient）-> Other
        let f = vec![0x11, 0x20, 0x10, 0x00, 0, 0, 0, 0];
        assert!(matches!(unmarshal(&f).unwrap(), Frame::Other));
    }
}
