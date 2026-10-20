# Finnish (fi) glossary

Word list so every string uses one term per concept. Kuma column differences are noted.

| English | Finnish | Note |
|---|---|---|
| monitor | seurain (pl. seuraimet, partitive seurainta) | Same as Kuma. `{n} seurain` / `{n} seurainta` |
| check (noun) | tarkistus | Kuma says "sydämenisku"; we avoid heartbeat wording |
| heartbeat | tarkistus | Not used as a separate term in the UI |
| uptime | käytettävyys | Kuma: "päälläoloaika"; ours is shorter and reads better with percentages |
| downtime | seisokki | |
| outage | katko | status "Outage" = Katko |
| response time | vasteaika | Short form "Vaste" in tight labels |
| incident | häiriö | Same as Kuma |
| maintenance | huolto | Kuma: "ylläpito"; huolto is the usual word for planned service work |
| maintenance window | huoltoikkuna | |
| status page | tilasivu | |
| notification channel | ilmoituskanava | "kanava" when short |
| notification | ilmoitus | |
| alert | hälytys | |
| escalation | eskalointi | |
| flapping | heilunta | |
| retry / retries | uusintayritys / uudelleenyritykset | "Uudelleenyritykset" for the retries setting |
| push monitor | push-seurain | "Push" stays as the type name |
| remote agent | etäagentti | |
| group | ryhmä | |
| tag | tunniste | Same as Kuma |
| badge | merkki | Status badge = tilamerkki |
| API token | API-tunnus | |
| token | tunnus | |
| two-factor authentication | kaksivaiheinen tunnistautuminen | Short: "kaksivaiheinen" |
| recovery code | palautuskoodi | |
| backup | varmuuskopio | Tab/nav: "Varmuuskopio" |
| restore | palauttaa / palautus | |
| dashboard | kojelauta | |
| settings | asetukset | |
| user | käyttäjä | |
| role | rooli | viewer = Katsoja, editor = Muokkaaja, admin = Ylläpitäjä |
| certificate | varmenne | |
| expiry | vanheneminen | "Vanhenee" |
| retention | säilytysaika | |
| paused | tauolla | |
| up | toimii | Status pill "Toimii" |
| down | alhaalla | |
| pending | odottaa | |
| unknown | tuntematon | |
| interval | aikaväli | |
| timeout | aikakatkaisu | |
| target | kohde | |
| keyword | avainsana | |
| webhook | webhook | Kept in English |
| Apprise | Apprise | Product name |
| discovery | löytäminen | "Docker-löytäminen"; verb "löytää" |
| import / export | tuonti / vienti | verbs: tuoda / viedä |
| configuration as code | asetukset koodina | |
| session | istunto | |
| language | kieli | |
| time zone | aikavyöhyke | |
| public URL | julkinen URL | |
| draft | luonnos | |
| published | julkaistu | |
| section | osio | |
| resolver | selvitin | DNS resolver |
| log in / log out | kirjaudu sisään / kirjaudu ulos | |
| password | salasana | |
| username | käyttäjätunnus | "user name" = käyttäjänimi |
| recovery / authenticator app | todennussovellus | |
| response | vaste | |

## Style
- Address: neutral. Imperatives and infinitives ("Valitse seuraimet", "Lisää kanava"), never "te"; "sinä" forms only when unavoidable ("sinun").
- Quotation marks: ”…” (same mark on both sides). Apostrophe ’ not needed in Finnish.
- Capitalisation: sentence case; no capitalised nouns in the middle of a sentence.
- Compounds are written as one word (seuraintyyppi, ilmoituskanava); long labels are shortened.
- Numerals take the partitive singular after n > 1 in UI counts, which equals the `other` form; `one` = exactly 1.
- Dates: `20. lokakuuta 2026`, time `klo 14:30`, 24-hour clock.
