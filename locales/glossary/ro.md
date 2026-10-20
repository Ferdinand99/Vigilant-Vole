# Romanian (ro) glossary

Terms were decided once and are reused across `locales/ro.toml`. Kuma word list consulted only for recognition.

## Style
- Address: informal-polite singular imperative as in Apple's Romanian UI ("Introdu…", "Alege…"); never "dumneavoastră".
- Quotation marks „ ” (nested: « »). Typographic apostrophes and … kept as in English.
- Sentence case; no capitals for nouns in the middle of a sentence.
- Comma-below letters only: ș ț (U+0219, U+021B), never ș ț.
- Plurals: `one` (exactly 1), `few` (0 and numbers ending 01–19: "{n} monitoare"), `other` (20+: "{n} de monitoare").
- Dates: `20 octombrie 2026`, 24-hour time. Durations `{n} s`, `{n} min`, `{n} h`, `{n} z`.
- Brand and technical terms stay English (Vigilant Vole, Docker, Prometheus, TLS, HTTP, JSONPath, cron, DNS, API, URL …).

## Terms
| English | Romanian | Note |
|---|---|---|
| monitor | monitor (pl. monitoare) | same as Kuma |
| heartbeat | puls | Kuma keeps "Heartbeat"; rarely shown here |
| check (noun) | verificare | |
| to check | a verifica | |
| uptime | disponibilitate | Kuma: "Timpul de funcționare"; shorter for tight UI |
| downtime | indisponibilitate / timp de nefuncționare | "indisponibilitate" for labels |
| response time | timp de răspuns | |
| response | răspuns | |
| incident | incident | |
| outage | întrerupere | |
| maintenance | mentenanță | same as Kuma |
| maintenance window | fereastră de mentenanță | |
| status page | pagină de stare | Kuma: "Pagina de Status"; "stare" used for status everywhere |
| status | stare | |
| notification channel | canal de notificări | |
| notification | notificare | |
| alert | alertă | |
| escalation | escaladare | |
| flapping | oscilație | "Flap detection" = detectarea oscilațiilor |
| retry / retries | reîncercare / reîncercări | same as Kuma |
| push monitor | monitor push | |
| push URL | URL push | |
| remote agent | agent la distanță | |
| group | grup | |
| tag | etichetă | same as Kuma |
| badge | insignă | |
| API token | token API | |
| two-factor authentication | autentificare în doi pași | |
| authenticator app | aplicație de autentificare | |
| recovery code | cod de recuperare | |
| backup | copie de rezervă | Kuma uses "Backup"; Romanian phrase used for clarity |
| restore | restaurare / a restaura | |
| dashboard | panou | Kuma: "Panou de Control" |
| settings | setări | |
| user | utilizator | |
| username | nume de utilizator | |
| role | rol | |
| admin | administrator | "Admin" in tight spots |
| editor | editor | |
| viewer | cititor | |
| certificate | certificat | |
| expiry / expires | expirare / expiră | |
| retention | păstrare | "Păstrează istoricul verificărilor" |
| paused | în pauză | |
| up | disponibil | pair with "indisponibil" |
| down | indisponibil | Kuma: "Nefuncțional" |
| pending | în așteptare | same as Kuma |
| interval | interval | |
| timeout | timp limită | |
| target | țintă | |
| keyword | cuvânt cheie | same as Kuma |
| webhook | webhook | |
| Apprise | Apprise | |
| discovery | descoperire | |
| import / export | import / export | |
| configuration as code | configurare ca cod | |
| session | sesiune | |
| language | limbă | |
| time zone | fus orar | |
| site | site | |
| draft | ciornă | |
| published | publicat | |
| section | secțiune | |
| password | parolă | |
| log in / log out | autentificare / deconectare | "Autentifică-te" / "Deconectează-te" |
| sign out | deconectare | |
| operational | operațional | |
| degraded | performanță redusă | |
| recovery | revenire | |
| resolve (incident) | rezolvă | |
| pin | fixează | |
| merge / replace | îmbinare / înlocuire | |
| agent | agent | |
| container | container | |
| secret | secret | |
| severity | gravitate | |
| remote browser | browser la distanță | follows "agent la distanță" |
| real browser | browser real | |
| headless | headless | kept in English |
| screenshot | captură de ecran | |
| game server | server de joc | |
| server reflection | reflexia serverului | |
| metadata (gRPC) | metadate | |
| privacy password | parolă de confidențialitate | SNMPv3 |
| security level | nivel de securitate | |
| engine ID | ID de motor | |
| unlock | deblochează | |
| password-protected page | pagină protejată cu parolă | |
| page views | vizualizări de pagină | |
| unique visitors | vizitatori unici | |
| referrer | sursă de trafic / site de proveniență | |
| Tailscale peer | peer Tailscale | |
| MOTD | MOTD | kept in English |
| replica set | replicaSet | kept as parameter name |
| analytics | statistici | |
| cron expression | expresie cron | "cron" kept Latin |
