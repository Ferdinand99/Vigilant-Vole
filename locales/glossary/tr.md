# Turkish (tr) glossary

Word list for Vigilant Vole. Kuma column notes only where we differ from Uptime Kuma's Turkish words.

| English | Türkçe | Note |
|---|---|---|
| monitor | monitör | Kuma uses the same word. Singular after numbers (`{n} monitör`). |
| heartbeat | kalp atışı | Rarely shown; the UI says "kontrol" (check). |
| check | kontrol | Kuma: "servis testi". Shorter. |
| uptime | çalışma süresi | Kuma: "çalışma zamanı". |
| downtime | kesinti süresi | |
| outage | kesinti | |
| response time | yanıt süresi | Kuma: "cevap süresi". "Yanıt" is the neutral modern word. |
| incident | olay | Public announcement on a status page. |
| event | etkinlik | Entries in a monitor's history (avoids clash with "olay"). |
| maintenance | bakım | |
| maintenance window | bakım penceresi | |
| status page | durum sayfası | |
| notification channel | bildirim kanalı | |
| notification | bildirim | |
| alert | uyarı | |
| escalation | yükseltme | "Yükseltme kanalları". |
| flapping | dalgalanma | |
| retry / retries | yeniden deneme | |
| push monitor | push monitör | "Push" stays. |
| remote agent | uzak ajan | |
| group | grup | |
| tag | etiket | |
| badge | rozet | |
| API token | API belirteci | |
| secret | gizli anahtar | |
| two-factor authentication | iki adımlı doğrulama | Short: "iki adımlı". |
| authenticator app | doğrulama uygulaması | |
| recovery code | kurtarma kodu | |
| backup | yedek | Verb/action: "yedekle"; nav "Yedekleme". |
| restore | geri yükleme | |
| dashboard | panel | Same as Kuma. |
| settings | ayarlar | |
| user | kullanıcı | |
| username | kullanıcı adı | |
| password | parola | |
| role | rol | |
| admin / editor / viewer | yönetici / editör / görüntüleyici | |
| certificate | sertifika | |
| expiry / expires | son kullanma / sona erer | |
| retention | saklama | "Kontrol geçmişini şu süre sakla". |
| paused | duraklatıldı | Action: "duraklat" / "sürdür" (resume). Kuma: "durdur". |
| up | çalışıyor | Kuma: "aktif". |
| down | çalışmıyor | Kuma: "hizmet dışı" (too long for pills). |
| pending | bekliyor | |
| unknown | bilinmiyor | |
| interval | aralık | |
| timeout | zaman aşımı | |
| target | hedef | |
| keyword | anahtar sözcük | Kuma: "anahtar kelime". |
| webhook | webhook | Kept in English. |
| Apprise | Apprise | Product name. |
| discovery | keşif | "Docker keşfi". |
| import / export | içe aktar / dışa aktar | |
| configuration as code | kod olarak yapılandırma | |
| session | oturum | |
| language | dil | |
| time zone | saat dilimi | |
| log in / log out | giriş yap / çıkış yap | |
| sign out | oturumu kapat | Sessions context. |
| appearance | görünüm | |
| theme (auto/light/dark) | tema (otomatik/açık/koyu) | |
| severity | önem derecesi | |
| degraded | düşük performans | |
| operational | çalışıyor | Public page: "Tüm sistemler çalışıyor". |
| section | bölüm | |
| slug | adres kısaltması | |
| proxy | proxy | |
| redirect | yönlendirme | |
| header | başlık | HTTP header; "üstbilgi" avoided. |
| body | gövde | |
| resolver | çözümleyici | |
| container | kapsayıcı | Docker; "konteyner" avoided. |
| clone | çoğalt | "Kopyala" is used for copy-to-clipboard. |
| flapping alert | dalgalanma uyarısı | |
| remote browser | uzak tarayıcı | Headless Chromium the server connects to. |
| real browser | gerçek tarayıcı | Monitor type. |
| headless | başsız | "Başsız Chromium". |
| screenshot | ekran görüntüsü | |
| game server | oyun sunucusu | |
| server reflection | sunucu yansıtma | gRPC. |
| metadata | meta veri | gRPC call metadata. |
| privacy password | gizlilik parolası | SNMPv3. |
| security level | güvenlik düzeyi | SNMPv3; protocol names stay English. |
| engine ID | motor kimliği | |
| unlock | kilidi aç | Button on a password-protected page. |
| password-protected page | parola korumalı sayfa | Pill: "Parola korumalı". |
| page views | sayfa görüntüleme | Short: "görüntüleme". |
| unique visitors | tekil ziyaretçi | |
| referrer | yönlendiren | "Yönlendiren siteler". |
| Tailscale peer | Tailscale eşi | |
| MOTD | MOTD | Kept in English. |
| replica set | replicaSet | Connection-string option, kept in English. |

## Style

- Address: neutral polite imperative (Girin, Seçin, Ekleyin); never "sen" forms. Buttons use the short imperative (Kaydet, Sil, İptal).
- Quotation marks: “ ” and ’ as in English. Ellipsis … kept where English has it.
- Sentence case. Respect İ/ı. Month and weekday names are capitalised (Ekim, Salı).
- Suffixes are never attached to `{placeholders}`; sentences are reshaped ("Düzenle: {name}", "“{name}” silinsin mi?").
- Numbers: nouns stay singular after numerals (`{n} gün`), so `one` and `other` are identical.
- Product, protocol and technical terms stay English (HTTP, TLS, DNS, Docker, Prometheus ...).

| cron expression | cron ifadesi | "cron" kept Latin |
