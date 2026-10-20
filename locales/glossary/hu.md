# Hungarian (hu) glossary

Style
- Register: impersonal / infinitive, no "te", no stiff formal address. Buttons: bare imperative stem form as in common Hungarian software ("Mentés", "Törlés" as nouns; "Adja meg…" for prompts, "Adjon nevet…").
- Quotation marks: „ ” (low-high). Apostrophe/ellipsis: keep `…`; dashes as in English (— –).
- Sentence case, lowercase month and weekday names. Hungarian nouns stay singular after numerals ("3 monitor"), so plural keys have only `one` and `other`.
- Dates: `2026. október 20.`; time 24-hour. Avoid suffixes on `{placeholders}`; rephrase.
- Product, protocol and provider names stay in English.

| English | Hungarian | Note |
|---|---|---|
| monitor | monitor | Kuma mixes "figyelő"/"monitor"; we use "monitor" everywhere |
| heartbeat | életjel | as Kuma |
| check (noun) | ellenőrzés | Kuma: "vizsgálat" |
| check now | Ellenőrzés most | |
| uptime | elérhetőség | Kuma keeps "Uptime"; we use the Hungarian word everywhere |
| downtime | kiesés | |
| outage | kiesés | same as downtime; status "danger" badge also "Kiesés" |
| response time | válaszidő | |
| incident | incidens | |
| maintenance window | karbantartási ablak | |
| maintenance | karbantartás | |
| status page | állapotoldal | Kuma: "státusz oldal" |
| notification channel | értesítési csatorna | |
| notification | értesítés | |
| alert | riasztás | |
| escalation | eszkaláció | |
| flapping | billegés | |
| retry | újrapróbálkozás | |
| push monitor | push monitor | |
| remote agent | távoli ügynök | |
| group | csoport | |
| tag | címke | as Kuma |
| badge | jelvény | |
| API token | API-token | |
| two-factor authentication | kétlépcsős azonosítás | short: "kétlépcsős" |
| recovery code | helyreállítási kód | |
| backup | biztonsági mentés | tight labels: "Mentés" (nav) |
| restore | visszaállítás | |
| dashboard | irányítópult | |
| settings | beállítások | |
| user | felhasználó | |
| role | szerepkör | |
| admin / editor / viewer | Adminisztrátor / Szerkesztő / Megtekintő | "Admin" kept short in role.admin |
| certificate | tanúsítvány | |
| expiry | lejárat | |
| retention | megőrzés | |
| paused | szüneteltetve | pill: "Szünetel" |
| up | Működik | as Kuma |
| down | Leállt | as Kuma |
| pending | Függőben | as Kuma |
| interval | időköz | |
| timeout | időkorlát | |
| target | cél | |
| keyword | kulcsszó | |
| webhook | webhook | |
| Apprise | Apprise | |
| discovery | felderítés | |
| import / export | importálás / exportálás | |
| config as code | konfiguráció kódként | |
| session | munkamenet | |
| language | nyelv | |
| time zone | időzóna | |
| password | jelszó | |
| user name | felhasználónév | |
| sign in / log in | bejelentkezés | |
| log out | kijelentkezés | |
| section | szakasz | |
| draft | piszkozat | |
| published | közzétett | |
| pin | kitűzés | |
| resolve | lezárás | incident resolved: "Megoldva" |
| register (domain) | regisztráció | |
| response body | válasz törzse | |
| header | fejléc | |
| redirect | átirányítás | |
| proxy | proxy | |
| secret | titok | |
| agent | ügynök | |
| rotate token | token cseréje | |
| slug | URL-név | |
| theme | téma | |
| appearance | megjelenés | |
| remote browser | távoli böngésző | |
| real browser | valódi böngésző | monitor type "Valódi böngésző"; headless = "fej nélküli" |
| screenshot | képernyőkép | |
| game server | játékszerver | |
| server reflection | szerver-reflexió | gRPC |
| metadata | metaadatok | gRPC |
| privacy password | adatvédelmi jelszó | SNMPv3; privacy = "adatvédelem" |
| security level | biztonsági szint | |
| engine ID | motorazonosító | SNMPv3 |
| unlock | Feloldás | button on password page |
| password-protected page | jelszóval védett oldal | pill "Jelszóval védett" |
| page views | oldalmegtekintések | short: "megtekintések" |
| unique visitors | egyedi látogatók | |
| referrer | hivatkozó oldal | |
| Tailscale peer | Tailscale-partner | |
| analytics | analitika | |
| replica set | replicaSet | kept as MongoDB option name |

Decisions
- "Működik" (up) and "Leállt" (down) follow Kuma; "Függőben" for pending.
- Where an English string is split around a `<code>` element (e.g. `push.help_*`), the Hungarian is split to read correctly with the code inserted between the parts.
- OAuth2 field names (Client ID, Client secret, Scope, Audience) are kept in English as protocol terms.
