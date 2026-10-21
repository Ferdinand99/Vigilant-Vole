# Słownik / Glossary – Polski (pl)

Kuma = Uptime Kuma `pl.json`, used only as a word list.

| English | Polski | Note |
|---|---|---|
| monitor | monitor | m.; plural monitory / monitorów |
| heartbeat | sprawdzenie (wynik) | Kuma: "bicie serca"; we avoid it, UI says "check" |
| check | sprawdzenie | pl. sprawdzenia / sprawdzeń |
| uptime | dostępność | Kuma: "czas pracy"; "dostępność" is clearer for a percentage |
| downtime | przestój | |
| outage | awaria | status.danger, severity |
| response time | czas odpowiedzi | |
| incident | incydent | |
| maintenance | konserwacja | same as Kuma |
| maintenance window | okno konserwacji | |
| status page | strona statusu | same as Kuma |
| notification channel | kanał powiadomień | |
| notifications | powiadomienia | |
| alert | alert | |
| escalation | eskalacja | |
| flapping | niestabilność / niestabilny | "Flapping" label = "Niestabilny" |
| flap detection | wykrywanie niestabilności | |
| retry / retries | ponowienie / ponowienia | "Ponowienia przed awarią" |
| push monitor | monitor push | |
| push URL | adres push | |
| remote agent | zdalny agent | |
| group | grupa | |
| tag | tag | |
| badge | odznaka | |
| API token | token API | |
| two-factor authentication | uwierzytelnianie dwuskładnikowe | "dwuskładnikowe" in headings, "2FA" not used |
| recovery code | kod odzyskiwania | |
| authenticator app | aplikacja uwierzytelniająca | |
| backup | kopia zapasowa | Kuma: "Backup"; we use the Polish term |
| restore | przywracanie / przywróć | |
| dashboard | panel | same as Kuma |
| settings | ustawienia | |
| user | użytkownik | |
| username | nazwa użytkownika | |
| role | rola | |
| viewer | Obserwator | |
| editor | Edytor | |
| admin | Admin | short for narrow pills |
| certificate | certyfikat | |
| expiry | wygaśnięcie | |
| retention | przechowywanie | "Przechowuj historię sprawdzeń przez" |
| paused | wstrzymany | Kuma: "Wstrzymaj" for action; same |
| up | Działa | status/event |
| down | Niedostępny | status/event; "Down" in sentences: "jest niedostępny" |
| pending | Oczekuje | same as Kuma |
| interval | interwał | |
| timeout | limit czasu | |
| target | cel | |
| keyword | słowo kluczowe | same as Kuma |
| webhook | webhook | |
| Apprise | Apprise | product name |
| discovery | wykrywanie | "Wykrywanie Dockera" |
| import / export | import / eksport | verbs: Importuj / Eksportuj |
| configuration as code | konfiguracja jako kod | |
| session | sesja | |
| language | język | |
| time zone | strefa czasowa | same as Kuma |
| sign in / log in | zalogować | "Zaloguj" ; log out "Wyloguj" |
| remote probe location | lokalizacja | |
| resolver | resolver | DNS term kept |
| upside-down mode | tryb odwrócony | same as Kuma |
| draft / published | wersja robocza / opublikowana | |
| severity | ważność | |
| slug | slug | technical, kept |
| header | nagłówek | |
| body | treść | request body = "treść żądania" |
| remote browser | zdalna przeglądarka | |
| real browser | prawdziwa przeglądarka | monitor type; "monitory prawdziwej przeglądarki" |
| headless | bez interfejsu | "Chromium bez interfejsu" |
| screenshot | zrzut ekranu | |
| game server | serwer gier | |
| server reflection | refleksja serwera | |
| metadata | metadane | gRPC |
| privacy password | hasło prywatności | SNMPv3 |
| security level | poziom zabezpieczeń | SNMPv3 |
| engine ID | ID silnika | SNMPv3 |
| unlock | Odblokuj | button |
| password-protected page | strona chroniona hasłem | pill "Chroniona hasłem" |
| page views | wyświetlenia strony | |
| unique visitors | unikalni odwiedzający | |
| referrer | strona odsyłająca / źródło odwiedzin | heading "Główne źródła odwiedzin" |
| Tailscale peer | peer Tailscale | |
| MOTD | MOTD | kept |
| replica set | replicaSet | kept as parameter; "węzeł główny" = primary |

## Style

- Address: neutral imperatives and impersonal phrasing ("Wpisz nazwę", "Nie można zapisać"); no "Pan/Pani", "ty" avoided.
- Quotes: „…” (and »…« inside). Apostrophes not needed. Ellipsis `…` kept as in English.
- Names inserted into sentences are kept in the nominative inside quotes where the case would change: `Usuń: „{name}”?`.
- Sentence case; months and weekdays lower-case; dates "20 października 2026".
- Plurals: one / few / many / other (other = genitive singular form, e.g. "{n} monitora").
- Prefer short forms in tabs, buttons and pills ("Ponów" not "Spróbuj ponownie").
- Plural bases: `.few` (2–4, not 12–14) uses nominative plural ("2 użytkownicy", "3 monitory"); `.many` (0, 5–21, …) genitive plural ("5 użytkowników"); `.other` (fractions) genitive singular.
- Pointers to the Security page: "w sekcji Bezpieczeństwo" (nominative after "sekcji"), avoiding locative of page names. Same for "w sekcji Tokeny API".
- Avoid gendered forms for the viewer ("zalogowany", "wiedział"); use impersonal phrasing ("sesja pozostanie aktywna", "aby było wiadomo").
- Certificate/domain expiry in the past: "Termin minął {n} dni temu" (gender-neutral, serves cert and domain).
- Raw/plain text encoding: "zwykły tekst".

| cron expression | wyrażenie cron | "cron" kept Latin |
| subscriber | subskrybent | |
| subscribe / unsubscribe | subskrybuj / wypisz się | |
| confirm subscription | potwierdź subskrypcję | |
| webhook subscriber | subskrybent webhook | |
| pending (sign-up) | oczekuje | |
| maintenance started / ended | konserwacja rozpoczęta / zakończona | |
