# zhub

[Ziggurat](https://github.com/zigpy/ziggurat) Zigbee yığınını gömen, modemde çalışan
hafif bir Zigbee merkezi. Home Assistant ya da Zigbee2MQTT gerektirmez; tek bir statik
ikili ve içine gömülü bir web arayüzünden oluşur.

- Yeni ağ kurar, ağ anahtarını, çerçeve sayaçlarını ve yığın tablolarını
  `/opt/var/lib/zhub/state.json` dosyasında saklar.
- Katılan cihazları tanır (ZDO), kümelerini merkeze bağlar ve raporlamayı ayarlar.
- Işıklar: aç/kapa, parlaklık, renk sıcaklığı.
- Sensörler: sıcaklık, nem, varlık, IAS bölgeleri (kapı, hareket, su), pil.
- Web arayüzü: `http://<modem>:8099` (kimlik doğrulama yok; yalnızca yerel ağ).

## Mimari

Zigbee2MQTT'deki mantıkla: ana uygulama hiçbir cihaz türünü bilmez, cihazlar
`src/devices/` altında veri olarak tanımlanır.

- **Tanım** (`Definition`): parmak izi (model ya da üretici adı), kart türü,
  özellikler, ek bağlamalar, saat biçimi.
- **Özellik** (`Feature`): anahtar, etiket, tür (sayı/anahtar/seçim/program),
  erişim, arayüzdeki yeri ve kaynağı (ZCL özniteliği, ışık komutu ya da Tuya DP).
  Gelen değerin çözülmesi, yazma ve arayüz şeması aynı tanımdan gelir.
- **Ortak parçalar** (`devices/common.rs`): standart kümelerin özellikleri;
  tanımı olmayan cihazlar sundukları kümelere göre bunlardan otomatik tanım alır.
- Yapılandırma (bağlama, raporlama, okuma) özelliklerden türetilir; uyuyan
  cihazlara yazma kuyruğa alınır ve cihaz uyanınca yapılır.

Yeni cihaz eklemek: Z2M'deki tanımını (`zigbee-herdsman-converters`) açıp aynı
bilgileri `devices/<üretici>.rs` içine bir `Definition` olarak yazmak ve
`devices/mod.rs`'deki listeye eklemek.

Tanımlı cihazlar: SONOFF SNZB-02P, SONOFF TRVZB, Tuya ZWT07 (`_TZE200_g9a3awaj`).

## Donanım

Sonoff Dongle Plus MG24 + Silicon Labs OpenThread RCP 3.1.1 (921600 baud, akış
kontrolü yok). Keenetic'te "USB to Serial Port Converters" bileşeni kurulu olmalı.

## Derleme ve kurulum

```sh
./build.sh
cat target/mips-unknown-linux-musl/release/zhub | ssh -p 222 root@192.168.1.1 'cat > /opt/bin/zhub && chmod +x /opt/bin/zhub'
cat deploy/S99zhub | ssh -p 222 root@192.168.1.1 'cat > /opt/etc/init.d/S99zhub && chmod +x /opt/etc/init.d/S99zhub'
ssh -p 222 root@192.168.1.1 /opt/etc/init.d/S99zhub restart
```

Günlük: `/opt/var/log/zhub.log`.
