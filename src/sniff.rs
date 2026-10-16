//! Tanılama: seri porttan RCP'den gelen baytların bir kopyasını HDLC/Spinel olarak
//! çözer ve 802.15.4 MAC komut çerçevelerini adresleme alanlarıyla günlüğe yazar.
//! Ziggurat'a giden akışa dokunmaz. Radyo karışık moda alındığında radyonun normalde
//! süzdüğü çerçeveler de görünür.

use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};

use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tracing::info;

const HDLC_FLAG: u8 = 0x7E;
const HDLC_ESCAPE: u8 = 0x7D;
const SPINEL_CMD_PROP_VALUE_IS: u8 = 6;
const SPINEL_PROP_STREAM_RAW: u8 = 113;
const MAX_FRAME: usize = 2048;

pub struct Tap<T> {
    inner: T,
    frame: Vec<u8>,
    escaped: bool,
}

impl<T> Tap<T> {
    pub fn new(inner: T) -> Self {
        Self {
            inner,
            frame: Vec::new(),
            escaped: false,
        }
    }

    fn feed(&mut self, byte: u8) {
        match byte {
            HDLC_FLAG => {
                // Sondaki 2 bayt HDLC sağlama toplamı
                if self.frame.len() > 2 {
                    let end = self.frame.len() - 2;
                    if let Some(line) = describe_spinel(&self.frame[..end]) {
                        info!("SNIFF {line}");
                    }
                }
                self.frame.clear();
                self.escaped = false;
            }
            HDLC_ESCAPE => self.escaped = true,
            _ => {
                let value = if self.escaped { byte ^ 0x20 } else { byte };
                self.escaped = false;
                if self.frame.len() < MAX_FRAME {
                    self.frame.push(value);
                }
            }
        }
    }
}

impl<T: AsyncRead + Unpin> AsyncRead for Tap<T> {
    fn poll_read(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<io::Result<()>> {
        let before = buf.filled().len();
        let result = Pin::new(&mut self.inner).poll_read(cx, buf);
        if let Poll::Ready(Ok(())) = result {
            let received = buf.filled()[before..].to_vec();
            for byte in received {
                self.feed(byte);
            }
        }
        result
    }
}

impl<T: AsyncWrite + Unpin> AsyncWrite for Tap<T> {
    fn poll_write(mut self: Pin<&mut Self>, cx: &mut Context<'_>, data: &[u8]) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.inner).poll_write(cx, data)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

/// Spinel: başlık, komut, özellik (ikisi de 128'in altında, tek bayt), değer.
/// STREAM_RAW değeri: PSDU uzunluğu (LE), PSDU, RSSI, …
fn describe_spinel(frame: &[u8]) -> Option<String> {
    let [_header, command, property, value @ ..] = frame else {
        return None;
    };
    if *command != SPINEL_CMD_PROP_VALUE_IS || *property != SPINEL_PROP_STREAM_RAW {
        return None;
    }
    let len = usize::from(u16::from_le_bytes([*value.first()?, *value.get(1)?]));
    let psdu = value.get(2..2 + len)?;
    let rssi = *value.get(2 + len)? as i8;
    describe_mac_command(psdu, rssi)
}

fn address(bytes: &[u8], mode: u16) -> Option<(String, usize)> {
    match mode {
        0 => Some(("-".into(), 0)),
        2 => Some((format!("0x{:04x}", u16::from_le_bytes([*bytes.first()?, *bytes.get(1)?])), 2)),
        3 => {
            let raw = bytes.get(..8)?;
            let text = raw.iter().rev().map(|b| format!("{b:02x}")).collect::<Vec<_>>().join(":");
            Some((text, 8))
        }
        _ => None,
    }
}

/// Yalnızca MAC komut çerçeveleri: katılma ve yoklama burada görünür
pub fn describe_mac_command(psdu: &[u8], rssi: i8) -> Option<String> {
    let fcf = u16::from_le_bytes([*psdu.first()?, *psdu.get(1)?]);
    if fcf & 0x07 != 0x03 {
        return None;
    }
    let ack_request = (fcf >> 5) & 1;
    let pan_compression = (fcf >> 6) & 1;
    let dst_mode = (fcf >> 10) & 3;
    let version = (fcf >> 12) & 3;
    let src_mode = (fcf >> 14) & 3;
    let seq = *psdu.get(2)?;
    let mut offset = 3;

    let mut dst_pan = String::from("-");
    if dst_mode != 0 {
        dst_pan = format!("0x{:04x}", u16::from_le_bytes([*psdu.get(offset)?, *psdu.get(offset + 1)?]));
        offset += 2;
    }
    let (dst, used) = address(psdu.get(offset..)?, dst_mode)?;
    offset += used;
    if src_mode != 0 && pan_compression == 0 {
        offset += 2;
    }
    let (src, used) = address(psdu.get(offset..)?, src_mode)?;
    offset += used;
    let command = match *psdu.get(offset)? {
        0x01 => "AssociationRequest".to_owned(),
        0x02 => "AssociationResponse".to_owned(),
        0x04 => "DataRequest".to_owned(),
        0x07 => "BeaconRequest".to_owned(),
        other => format!("komut 0x{other:02x}"),
    };
    Some(format!(
        "{command} kaynak={src} hedef={dst} hedefPAN={dst_pan} sürüm={version} ackİstek={ack_request} sıra={seq} rssi={rssi}"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn describes_data_request_without_destination() {
        // FCF: komut, ACK iste, hedef adressiz, kaynak 64 bit (PAN sıkıştırması yok)
        let mut psdu = vec![0x23, 0xC0, 0x42, 0x13, 0x2B];
        psdu.extend_from_slice(&[0xeb, 0x5b, 0xb4, 0xfe, 0xff, 0x5f, 0xae, 0x0c]);
        psdu.push(0x04);
        let line = describe_mac_command(&psdu, -55).unwrap();
        assert!(line.starts_with("DataRequest kaynak=0c:ae:5f:ff:fe:b4:5b:eb hedef=-"), "{line}");
    }

    #[test]
    fn describes_association_request_to_coordinator() {
        // FCF 0xc823: komut, ACK iste, hedef kısa, kaynak 64 bit
        let mut psdu = vec![0x23, 0xC8, 0x10, 0x13, 0x2B, 0x00, 0x00, 0xFF, 0xFF];
        psdu.extend_from_slice(&[0xeb, 0x5b, 0xb4, 0xfe, 0xff, 0x5f, 0xae, 0x0c]);
        psdu.extend_from_slice(&[0x01, 0x80]);
        let line = describe_mac_command(&psdu, -40).unwrap();
        assert!(line.contains("AssociationRequest kaynak=0c:ae:5f:ff:fe:b4:5b:eb hedef=0x0000 hedefPAN=0x2b13"), "{line}");
    }

    #[test]
    fn ignores_data_frames() {
        assert!(describe_mac_command(&[0x41, 0x88, 0x01], 0).is_none());
    }

    #[test]
    fn tap_decodes_escaped_stream_raw_frame() {
        let mut tap = Tap::new(());
        // Spinel: başlık 0x80, PROP_VALUE_IS, STREAM_RAW, uzunluk 4, PSDU, RSSI; 0x7E kaçışlı
        let body = [0x80, 6, 113, 4, 0, 0x23, 0xC8, 0x7E, 0x00, 0xC4, 0xAA, 0xBB];
        let mut wire = vec![HDLC_FLAG];
        for b in body {
            if b == HDLC_FLAG || b == HDLC_ESCAPE {
                wire.extend_from_slice(&[HDLC_ESCAPE, b ^ 0x20]);
            } else {
                wire.push(b);
            }
        }
        for b in wire {
            tap.feed(b);
        }
        assert_eq!(tap.frame[..6], [0x80, 6, 113, 4, 0, 0x23]);
        assert_eq!(tap.frame[7], 0x7E);
    }
}
