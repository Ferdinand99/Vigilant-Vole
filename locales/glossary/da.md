# Vigilant Vole – ordliste (Dansk, `da`)

Kilde: `locales/en.toml`. Ordforråd er sammenholdt med Uptime Kumas danske ord kun som ordliste (Kuma-filen kunne ikke hentes under arbejdet, så valgene er egne og sædvanlig dansk it-sprogbrug). Alle tekster er skrevet fra vores engelske kilde.

## Stil
- Tiltale: uformelt "du", aldrig "De". Naturlig imperativ: "Angiv et navn", "Vælg en gruppe". Kortfattet, roligt tonefald.
- Citationstegn: »…« (dansk stil), indeni ›…‹. Apostrof og tankestreg som i kilden (— → –/—, `…` bevares).
- Store/små bogstaver: kun første ord med stort (sætningsstil); månedsnavne, ugedage og sprognavne med småt.
- Køn: overvågning (en), tjek (et), statusside (en), kanal (en), hændelse (en), begivenhed (en), nedbrud (et), vindue (et), mål (et), certifikat (et), interval (et), token (et), tag (et), badge (en).
- Tal: `.one` for præcis 1, `.other` for alt andet (også 0). Intet ulige rod: "{n} tjek" er ens i ental/flertal.
- Fagord bevares: HTTP, TLS, DNS, TCP, API, URL, YAML, JSON, cron, Docker, Prometheus, Grafana, Apprise osv.

## Kerneord
| English | Dansk | Note |
|---|---|---|
| monitor | overvågning (en) | Kuma bruger samme ord; flertal "overvågninger" |
| heartbeat | tjek / hjerteslag | bruges stort set kun som "tjek" |
| check | tjek (et) | flertal "tjek"; "kontrol" undgås |
| uptime | oppetid | |
| downtime | nedetid | |
| response time | svartid | |
| outage | nedbrud (et) | status "Outage" = "Nedbrud" |
| event | begivenhed | hændelsesloggen på overvågningen |
| incident | hændelse (en) | meddelelser på statussiden |
| maintenance window | vedligeholdelsesvindue | |
| maintenance | vedligeholdelse | |
| status page | statusside (en) | |
| notification channel | notifikationskanal | kort: "kanal" |
| notifications | notifikationer | |
| alert | alarm (en) | "advarsel" er kun status Warning |
| escalation | eskalering | |
| flapping | ustabil | "registrering af ustabilitet" for flap detection |
| retry / retries | nyt forsøg / nye forsøg | |
| push monitor | push-overvågning | |
| remote agent | fjernagent (en) | |
| group | gruppe | |
| tag | tag (et) | flertal "tags" |
| badge | badge (en) | flertal "badges" |
| API token | API-token (et) | |
| two-factor authentication | tofaktorgodkendelse | kort: "tofaktor" |
| recovery code | gendannelseskode | |
| authenticator app | autentificeringsapp | |
| backup | backup (en) | |
| restore | gendan / gendannelse | |
| dashboard | dashboard (et) | |
| settings | indstillinger | |
| user | bruger | |
| role | rolle | |
| viewer | Læser | |
| editor | Redaktør | |
| admin | Admin | |
| certificate | certifikat | |
| expiry / expires | udløb / udløber | |
| retention | opbevaring | "Opbevar tjekhistorik i" |
| paused | pauset | handling: "Sæt på pause" → kort "Pause" |
| resume | genoptag | |
| up | Oppe | |
| down | Nede | |
| pending | Afventer | |
| interval | interval | |
| timeout | timeout | |
| target | mål | |
| keyword | nøgleord | |
| webhook | webhook (en) | |
| Apprise | Apprise | uændret |
| discovery | registrering | "Docker-registrering" |
| import / export | import / eksport | verber: importér / eksportér |
| config as code | konfiguration som kode | |
| session | session | |
| language | sprog | |
| time zone | tidszone | |
| severity | alvorlighed | |
| degraded | forringet | |
| operational | i drift | |
| resolved | løst | |
| pin | fastgør | |
| draft | kladde | |
| published | udgivet | |
| section | afsnit | på statussiden |
| label | etiket | Docker-label: "label" bevares |
| secret | hemmelighed | |
| password | adgangskode | |
| username | brugernavn | |
| sign in / log in | log ind | "log ud" for log out |
| revoke | tilbagekald | |
| rotate (token) | roter | |
| delete / remove | slet / fjern | slet = permanent, fjern = tag ud af liste |
| preview | forhåndsvis | |
| layout/theme | tema | |
| remote browser | fjernbrowser | flertal "fjernbrowsere"; passer til "fjernagent" |
| real browser | rigtig browser | "Overvågninger med rigtig browser" |
| headless | headless | bevares (Chromium) |
| screenshot | skærmbillede | |
| game server | spilserver | |
| server reflection | server reflection | bevares som teknisk term |
| metadata | metadata | |
| privacy password (SNMPv3) | privatlivsadgangskode | "Privatliv (kryptering)" for priv-protokol |
| security level | sikkerhedsniveau | |
| engine ID | engine-ID | |
| unlock | lås op | |
| password-protected page | adgangskodebeskyttet side | pille: "Adgangskodebeskyttet" |
| page views | sidevisninger | "visninger" i korte mærkater |
| unique visitors | unikke besøgende | |
| referrer | henviser | flertal "henvisere" |
| analytics | statistik | fane/overskrift; "Google Analytics" bevares |
| Tailscale peer | Tailscale-peer | |
| MOTD | MOTD | bevares |
| replica set | replicaSet | bevares som parameter |
| site ID | websted-ID | |

- cron expression: cron-udtryk (the word "cron" stays Latin).
- subscriber: abonnent; subscribe / unsubscribe: abonnér / afmeld; confirm subscription: bekræft abonnement; webhook subscriber: webhook-abonnent; pending (sign-up): afventer; maintenance started / ended: vedligeholdelse startet / afsluttet.
