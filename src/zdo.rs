//! ZDO (Zigbee Device Object) istekleri ve yanıtları. Fonksiyonlar baştaki işlem
//! sıra numarası (TSN) olmadan çalışır; onu gönderen taraf ekler ve ayıklar.

use anyhow::{Result, bail};

use crate::store::Endpoint;

pub mod cluster {
    pub const NWK_ADDR_REQ: u16 = 0x0000;
    pub const IEEE_ADDR_REQ: u16 = 0x0001;
    pub const NODE_DESC_REQ: u16 = 0x0002;
    pub const SIMPLE_DESC_REQ: u16 = 0x0004;
    pub const ACTIVE_EP_REQ: u16 = 0x0005;
    pub const MATCH_DESC_REQ: u16 = 0x0006;
    pub const DEVICE_ANNCE: u16 = 0x0013;
    pub const BIND_REQ: u16 = 0x0021;
    pub const MGMT_LEAVE_REQ: u16 = 0x0034;
    pub const MGMT_PERMIT_JOINING_REQ: u16 = 0x0036;
    pub const RESPONSE: u16 = 0x8000;
}

pub const STATUS_SUCCESS: u8 = 0x00;

const MAC_CAP_RX_ON_WHEN_IDLE: u8 = 0x08;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NodeDescriptor {
    /// 0 koordinatör, 1 yönlendirici, 2 uç cihaz
    pub logical_type: u8,
    pub mac_capabilities: u8,
    pub manufacturer_code: u16,
}

impl NodeDescriptor {
    pub fn rx_on_when_idle(&self) -> bool {
        self.mac_capabilities & MAC_CAP_RX_ON_WHEN_IDLE != 0
    }
}

fn check_status(payload: &[u8]) -> Result<&[u8]> {
    match payload.first() {
        Some(&STATUS_SUCCESS) => Ok(&payload[1..]),
        Some(&status) => bail!("ZDO durum kodu 0x{status:02x}"),
        None => bail!("boş ZDO yanıtı"),
    }
}

fn u16_at(data: &[u8], offset: usize) -> Result<u16> {
    match data.get(offset..offset + 2) {
        Some(bytes) => Ok(u16::from_le_bytes([bytes[0], bytes[1]])),
        None => bail!("kısa ZDO yanıtı"),
    }
}

pub fn node_desc_req(nwk: u16) -> Vec<u8> {
    nwk.to_le_bytes().to_vec()
}

pub fn parse_node_desc_rsp(payload: &[u8]) -> Result<NodeDescriptor> {
    let rest = check_status(payload)?;
    let desc = rest.get(2..2 + 13).ok_or_else(|| anyhow::anyhow!("kısa düğüm tanımlayıcısı"))?;
    Ok(NodeDescriptor {
        logical_type: desc[0] & 0x07,
        mac_capabilities: desc[2],
        manufacturer_code: u16::from_le_bytes([desc[3], desc[4]]),
    })
}

pub fn active_ep_req(nwk: u16) -> Vec<u8> {
    nwk.to_le_bytes().to_vec()
}

pub fn parse_active_ep_rsp(payload: &[u8]) -> Result<Vec<u8>> {
    let rest = check_status(payload)?;
    let count = usize::from(*rest.get(2).ok_or_else(|| anyhow::anyhow!("kısa yanıt"))?);
    match rest.get(3..3 + count) {
        Some(eps) => Ok(eps.to_vec()),
        None => bail!("kısa uç nokta listesi"),
    }
}

pub fn simple_desc_req(nwk: u16, endpoint: u8) -> Vec<u8> {
    let mut out = nwk.to_le_bytes().to_vec();
    out.push(endpoint);
    out
}

pub fn parse_simple_desc_rsp(payload: &[u8]) -> Result<Endpoint> {
    let rest = check_status(payload)?;
    // nwk(2) uzunluk(1) ardından tanımlayıcı
    let desc = rest.get(3..).ok_or_else(|| anyhow::anyhow!("kısa yanıt"))?;
    let id = *desc.first().ok_or_else(|| anyhow::anyhow!("kısa tanımlayıcı"))?;
    let profile = u16_at(desc, 1)?;
    let device_id = u16_at(desc, 3)?;
    let mut offset = 6;
    let read_list = |offset: &mut usize| -> Result<Vec<u16>> {
        let count = usize::from(*desc.get(*offset).ok_or_else(|| anyhow::anyhow!("kısa liste"))?);
        *offset += 1;
        let list = (0..count)
            .map(|i| u16_at(desc, *offset + 2 * i))
            .collect::<Result<Vec<_>>>()?;
        *offset += 2 * count;
        Ok(list)
    };
    let in_clusters = read_list(&mut offset)?;
    let out_clusters = read_list(&mut offset)?;
    Ok(Endpoint {
        id,
        profile,
        device_id,
        in_clusters,
        out_clusters,
    })
}

/// Device_annce: nwk, ieee (küçük-uçlu), yetenek baytı
pub fn parse_device_annce(payload: &[u8]) -> Result<(u16, [u8; 8], u8)> {
    if payload.len() < 11 {
        bail!("kısa Device_annce");
    }
    let nwk = u16::from_le_bytes([payload[0], payload[1]]);
    let ieee: [u8; 8] = payload[2..10].try_into()?;
    Ok((nwk, ieee, payload[10]))
}

pub fn annce_rx_on_when_idle(capability: u8) -> bool {
    capability & MAC_CAP_RX_ON_WHEN_IDLE != 0
}

/// Cihazın kümesini koordinatörün uç noktasına bağlar
pub fn bind_req(src_ieee: [u8; 8], src_ep: u8, cluster: u16, dst_ieee: [u8; 8], dst_ep: u8) -> Vec<u8> {
    let mut out = src_ieee.to_vec();
    out.push(src_ep);
    out.extend_from_slice(&cluster.to_le_bytes());
    out.push(0x03); // 64 bit adres + uç nokta
    out.extend_from_slice(&dst_ieee);
    out.push(dst_ep);
    out
}

pub fn parse_status_rsp(payload: &[u8]) -> Result<()> {
    check_status(payload).map(|_| ())
}

pub fn mgmt_leave_req(ieee: [u8; 8]) -> Vec<u8> {
    let mut out = ieee.to_vec();
    out.push(0x00); // yeniden katılma yok, çocukları bırakma
    out
}

pub fn mgmt_permit_joining_req(duration: u8) -> Vec<u8> {
    vec![duration, 0x01]
}

/// Match_Desc_req'teki ilgilenilen adres; cihaz koordinatörü ararken 0x0000 ya da
/// yayın adresi kullanır
pub fn parse_match_desc_nwk(payload: &[u8]) -> Option<u16> {
    Some(u16::from_le_bytes([*payload.first()?, *payload.get(1)?]))
}

pub fn parse_nwk_of_interest(payload: &[u8]) -> Option<u16> {
    parse_match_desc_nwk(payload)
}

pub fn parse_nwk_addr_req_ieee(payload: &[u8]) -> Option<[u8; 8]> {
    payload.get(..8)?.try_into().ok()
}

// Koordinatörün kendisi hakkındaki sorulara verdiği yanıtlar

pub fn our_node_desc_rsp(nwk: u16) -> Vec<u8> {
    let mut out = vec![STATUS_SUCCESS];
    out.extend_from_slice(&nwk.to_le_bytes());
    out.extend_from_slice(&[
        0x00, // mantıksal tür: koordinatör
        0x40, // 2.4 GHz
        0x8F, // alternatif PAN koordinatörü, FFD, şebeke, alıcı açık, adres atar
        0x02, 0x10, // üretici kodu 0x1002 (Silicon Labs)
        0x52, // en büyük tampon
        0x80, 0x00, // en büyük gelen aktarım
        0x41, 0x2C, // sunucu maskesi: birincil güven merkezi, ağ yöneticisi, rev 22
        0x80, 0x00, // en büyük giden aktarım
        0x00,
    ]);
    out
}

pub fn our_active_ep_rsp(nwk: u16, endpoint: u8) -> Vec<u8> {
    let mut out = vec![STATUS_SUCCESS];
    out.extend_from_slice(&nwk.to_le_bytes());
    out.extend_from_slice(&[1, endpoint]);
    out
}

pub fn our_simple_desc_rsp(nwk: u16, endpoint: u8) -> Vec<u8> {
    let in_clusters: [u16; 2] = [0x0000, 0x0003];
    let out_clusters: [u16; 7] = [0x0001, 0x0006, 0x0008, 0x0300, 0x0402, 0x0405, 0x0500];
    let mut desc = vec![endpoint];
    desc.extend_from_slice(&0x0104u16.to_le_bytes());
    desc.extend_from_slice(&0x0400u16.to_le_bytes()); // IAS merkezi kontrol birimi
    desc.push(0x00);
    desc.push(in_clusters.len() as u8);
    desc.extend(in_clusters.iter().flat_map(|c| c.to_le_bytes()));
    desc.push(out_clusters.len() as u8);
    desc.extend(out_clusters.iter().flat_map(|c| c.to_le_bytes()));

    let mut out = vec![STATUS_SUCCESS];
    out.extend_from_slice(&nwk.to_le_bytes());
    out.push(desc.len() as u8);
    out.extend_from_slice(&desc);
    out
}

pub fn our_match_desc_rsp(nwk: u16, endpoint: u8) -> Vec<u8> {
    let mut out = vec![STATUS_SUCCESS];
    out.extend_from_slice(&nwk.to_le_bytes());
    out.extend_from_slice(&[1, endpoint]);
    out
}

pub fn our_addr_rsp(ieee: [u8; 8], nwk: u16) -> Vec<u8> {
    let mut out = vec![STATUS_SUCCESS];
    out.extend_from_slice(&ieee);
    out.extend_from_slice(&nwk.to_le_bytes());
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_hue_simple_descriptor() {
        // durum, nwk, uzunluk, ep 11, HA profili, renkli ışık, sürüm,
        // 2 giriş kümesi (OnOff, Level), 1 çıkış kümesi (OTA)
        let payload = [
            0x00, 0x34, 0x12, 0x0E, 0x0B, 0x04, 0x01, 0x0D, 0x01, 0x01, 0x02, 0x06, 0x00, 0x08,
            0x00, 0x01, 0x19, 0x00,
        ];
        let ep = parse_simple_desc_rsp(&payload).unwrap();
        assert_eq!(ep.id, 11);
        assert_eq!(ep.profile, 0x0104);
        assert_eq!(ep.device_id, 0x010D);
        assert_eq!(ep.in_clusters, [0x0006, 0x0008]);
        assert_eq!(ep.out_clusters, [0x0019]);
    }

    #[test]
    fn parses_node_descriptor_of_sleepy_end_device() {
        let payload = [
            0x00, 0x34, 0x12, 0x02, 0x40, 0x80, 0x02, 0x10, 0x50, 0xA0, 0x00, 0x00, 0x2C, 0xA0,
            0x00, 0x00,
        ];
        let desc = parse_node_desc_rsp(&payload).unwrap();
        assert_eq!(desc.logical_type, 2);
        assert!(!desc.rx_on_when_idle());
        assert_eq!(desc.manufacturer_code, 0x1002);
    }

    #[test]
    fn reports_failed_status() {
        assert!(parse_active_ep_rsp(&[0x81, 0x34, 0x12]).is_err());
    }

    #[test]
    fn our_simple_descriptor_round_trips() {
        let ep = parse_simple_desc_rsp(&our_simple_desc_rsp(0x0000, 1)).unwrap();
        assert_eq!(ep.id, 1);
        assert!(ep.out_clusters.contains(&0x0500));
    }
}
