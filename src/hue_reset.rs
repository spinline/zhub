//! Philips Hue lambalarını seri numarasıyla fabrika ayarına döndürme. Hue Bridge'in
//! "seri numarasıyla ekle" özelliğinin kullandığı Signify'a özel inter-PAN komutu;
//! zigbee-herdsman-converters'taki `philips_hue_factory_reset` eylemiyle aynı çerçeve.
//! Lamba yakınlık aramadan, numarası eşleşirse sıfırlanır.

use anyhow::{Result, bail};

use ziggurat_driver::ziggurat_ieee_802154::types::Eui64;

/// Touchlink kanalları önce, sonra kalanlar (Z2M ile aynı sıra)
pub const CHANNELS: [u8; 16] = [11, 15, 20, 25, 12, 13, 14, 16, 17, 18, 19, 21, 22, 23, 24, 26];

const MAC_FRAME_CONTROL: u16 = 0x0001 | 0x0800 | 0xC000; // veri, hedef kısa, kaynak 64 bit
const STUB_NWK_FRAME_CONTROL: u16 = 0x000B; // inter-PAN
const INTERPAN_APS_BROADCAST: u8 = 0x08 | 0x03;
const HUE_PAIRING_CLUSTER: u16 = 0x1000;
const TOUCHLINK_PROFILE: u16 = 0xC05E;
const SIGNIFY_MANUFACTURER_CODE: u16 = 0x100B;
const HUE_RESET_REQUEST: u8 = 0x00;
/// Kümeye özel + üreticiye özel + varsayılan yanıt yok
const ZCL_FRAME_CONTROL: u8 = 0x01 | 0x04 | 0x10;

/// "A1B2C3" biçimindeki 6 haneli onaltılık seri numarası
pub fn parse_serial(text: &str) -> Result<u32> {
    let text = text.trim();
    if text.len() != 6 || !text.chars().all(|c| c.is_ascii_hexdigit()) {
        bail!("seri numarası 6 haneli olmalı (0-9, A-F; O harfi yok, 0 rakamı var): {text}");
    }
    Ok(u32::from_str_radix(text, 16)?)
}

/// Yayınlanacak 802.15.4 çerçevesi (FCS'siz)
pub fn reset_frame(seq: u8, pan_id: u16, source: &Eui64, extended_pan_id: &Eui64, serials: &[u32]) -> Vec<u8> {
    let mut frame = Vec::with_capacity(64);
    frame.extend_from_slice(&MAC_FRAME_CONTROL.to_le_bytes());
    frame.push(seq);
    frame.extend_from_slice(&0xFFFFu16.to_le_bytes()); // hedef PAN
    frame.extend_from_slice(&0xFFFFu16.to_le_bytes()); // hedef adres
    frame.extend_from_slice(&pan_id.to_le_bytes());
    frame.extend_from_slice(&source.0);
    frame.extend_from_slice(&STUB_NWK_FRAME_CONTROL.to_le_bytes());
    frame.push(INTERPAN_APS_BROADCAST);
    frame.extend_from_slice(&HUE_PAIRING_CLUSTER.to_le_bytes());
    frame.extend_from_slice(&TOUCHLINK_PROFILE.to_le_bytes());
    frame.push(ZCL_FRAME_CONTROL);
    frame.extend_from_slice(&SIGNIFY_MANUFACTURER_CODE.to_le_bytes());
    frame.push(0); // ZCL sıra numarası
    frame.push(HUE_RESET_REQUEST);
    frame.extend_from_slice(&extended_pan_id.0);
    frame.push(serials.len() as u8);
    for serial in serials {
        frame.extend_from_slice(&serial.to_le_bytes());
    }
    frame
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_serials() {
        assert_eq!(parse_serial("a1b2c3").unwrap(), 0xA1B2C3);
        assert_eq!(parse_serial(" 0F00E1 ").unwrap(), 0x0F00E1);
        assert!(parse_serial("A1B2C").is_err());
        assert!(parse_serial("A1B2CO").is_err());
    }

    #[test]
    fn builds_the_same_frame_as_zigbee_herdsman() {
        let source = Eui64([0x1c, 0x48, 0x99, 0xfe, 0xff, 0xb1, 0x5c, 0x6c]);
        let ext_pan = Eui64([0xb7, 0x4a, 0x00, 0xc7, 0xa9, 0x26, 0x68, 0xca]);
        let frame = reset_frame(7, 0x2b13, &source, &ext_pan, &[0xA1B2C3]);
        assert_eq!(
            frame,
            [
                0x01, 0xC8, 0x07, // MAC FCF, sıra
                0xFF, 0xFF, 0xFF, 0xFF, // hedef PAN, hedef adres
                0x13, 0x2B, // kaynak PAN
                0x1c, 0x48, 0x99, 0xfe, 0xff, 0xb1, 0x5c, 0x6c, // kaynak IEEE
                0x0B, 0x00, // stub NWK
                0x0B, 0x00, 0x10, 0x5E, 0xC0, // APS: yayın, küme 0x1000, profil 0xC05E
                0x15, 0x0B, 0x10, 0x00, 0x00, // ZCL: FC, Signify, sıra, komut
                0xb7, 0x4a, 0x00, 0xc7, 0xa9, 0x26, 0x68, 0xca, // geniş PAN
                0x01, 0xC3, 0xB2, 0xA1, 0x00, // 1 seri numarası
            ]
        );
    }
}
