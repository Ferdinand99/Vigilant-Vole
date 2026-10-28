# Push fra serverne (VV, KK, Wren) via Postal Pigeon – plan

> **For agentic workers:** REQUIRED SUB-SKILL: superpowers:subagent-driven-development. Én oppgave om gangen, i rekkefølge.

**Mål:** Når VV, KK eller Wren sender et varsel, kommer det som push-varsel i Hedgerow-appen. Teksten skal være ekte («Monitor *api* er nede»), men verken Pigeon eller Apple skal se den.

**Arkitektur:**
- Appen lager én *handle* hos Pigeon per tilkobling og gir den til serveren.
- Serveren behandler «Hedgerow-appen» som en ny varslingskanal i vole-kit. Den følger samme regler som e-post, Discord og så videre: monitorkoblinger i VV, varselregler i KK og alle kanaler i Wren.
- Når kanalen fyrer, lagrer serveren teksten under en tilfeldig `ref` og sender `POST /v1/push` til Pigeon.
- En Notification Service Extension i appen våkner, henter teksten fra brukerens egen server med tokenet fra den delte nøkkelringen, og skriver om varselet.

**Spec:**
- Pigeon `docs/api.md` (Instances, Sending, What the push contains) og `docs/design.md` §7 (hvorfor teksten hentes på enheten) og §14 (push er av som standard).
- vole-kit: `kit/src/notify/` (kanaler og leverandører), `kit/src/crypto.rs` (forsegling), `kit/src/db/api_tokens.rs` (tokens), `kit/src/net.rs` og `kit/src/notify/http.rs` (beskyttet HTTP).

## Beslutninger (mine, eieren godkjenner planen)

1. **Push er en kanal, ikke et eget system.** Kanaltypen er `hedgerow` («Hedgerow-appen»). Admin legger den til under Varsler én gang og kobler den til monitorer og regler som andre kanaler. Uten en slik kanal er push av, som Pigeon-designet krever.
2. **Mottakere:**
   - Alle enheter som har registrert seg på serveren, får det som går til kanalen.
   - Ingen per-bruker-filter i v1. Alle roller kan uansett se alt i VV, KK og Wren.
   - En enhet hører til en bruker. Sletter du brukeren, eller tokenet enheten registrerte seg med, forsvinner handlene.
3. **Lesetokens får registrere sin egen enhet.** Appens tokens er ofte bare lesetokens. Registrering endrer ikke serverens data, bare brukerens egen enhetsliste, så kit-vakten får et smalt unntak for akkurat disse rutene. Uten dette måtte alle lage nye tokens.
4. **Pigeon-registrering skjer av seg selv.** Første gang en `hedgerow`-kanal skal brukes, henter serveren en challenge og løser PoW-en i `spawn_blocking` (cirka 1 s). Så registrerer den seg og lagrer `hrk_…` forseglet (`crypto::seal`, formål `push-relay`) i `settings`.
   - Relay-adressen er en innstilling. Standard er `https://pigeon.hedgerow.tools`, eller env `KIT_PUSH_RELAY_URL` for den som kjører sin egen.
5. **Teksten blir på serveren:**
   - Hvert varsel lagres i `push_events (ref, title, body, kind, level, created_at)` i 24 timer. `ref` er 16 tilfeldige byte i base64url.
   - Teksten er den samme ferdig oversatte teksten som `Alert` allerede har, på kanalens språk.
   - Ingen URL lagres der: appen åpner aldri noe fra et push.
6. **Levering:**
   - Én `POST /v1/push` per varsel, med opptil 25 handles per kall, `kind: alert`, `ref`, `ttl` 3600 og `event_id` = ny UUID per varsel og kall.
   - `gone` gir slettet handle. `limited` og `unavailable` blir logget og gitt opp, med samme mønster for nye forsøk som produktet har i dag (KK har kø, VV og Wren har ingen).
   - `401 unknown_instance` gir én ny registrering. `403 suspended` blir logget som feil, og kanalen stopper.
7. **Appen:**
   - Handles lages bare når Varsler er på i appen. Det skjer én gang per tilkobling, og en bryter per tilkobling lar deg slå den av.
   - `tag` (16 tilfeldige byte) kobler push til tilkoblingen. Koblingen ligger i App Group, uten hemmeligheter.
   - Fjernes en tilkobling, eller slås bryteren av, avregistreres handle-en hos serveren og trekkes tilbake hos Pigeon.

## Global Constraints

- vole-kit:
  - Alle nye strenger finnes på alle 26 språk; testen `assert_full_coverage` skal være grønn.
  - Migrasjoner ligger i kit-kjernen, så VV, KK og Wren får tabellene likt.
  - Hemmeligheter er forseglet: instans-credential og handles (`hrh_…`) sammen med SHA-256 for oppslag.
  - Ingen nye avhengigheter utover det kit-en har (sha2, aws-lc-rs, reqwest, uuid og rand finnes fra før; sjekk).
- All utgående trafikk går gjennom kit-ens beskyttede klient (`notify/http.rs`). Relay-svaret har en grense på 64 KiB.
- Server-API-et svarer med samme feilformat som kit-API-et. Nye ruter står i API-dokumentasjonen (`RouteDoc`, OpenAPI).
- Appen bruker samme regler som før: https eller LAN, ingen omdirigering til annen vert, maks 2 MiB, token bare i nøkkelringen, servertekst er ren tekst, en og nb.
- Appen skal aldri handle på innholdet i et push. Den gjør bare dette: slår opp `tag` til en tilkobling, henter `ref` fra den tilkoblingens egen server og viser teksten. Ukjent `hr.v` blir behandlet som «hent på nytt».

## Review Focus

1. **Lesetoken-unntaket** skal ikke slippe gjennom noe annet enn de tre push-rutene (Task 1).
2. **En server som ikke er registrert hos Pigeon**, er nede eller er suspendert, skal ikke henge varslingsløkka. Push har en tidsgrense, og de andre kanalene går som før (Task 1).
3. **Mange enheter** (over 25 handles) deles i flere kall, og `gone` rydder opp. Kit-ens retry skal ikke sende samme `ref` to ganger uten samme `event_id` (Task 1).
4. **NSE-ens tidsfrist** (cirka 30 s, lite minne): hentingen avbrytes etter 10 s, og da står den generelle teksten. Ingen krasj når tokenet mangler (appen er låst og nøkkelringen utilgjengelig: bruk `AfterFirstUnlock`) (Task 4).
5. **En tilkobling som fjernes**, gir ingen foreldreløse handles: serveren avregistrerer, Pigeon trekker tilbake, og et senere `gone` rydder resten (Task 4).

---

### Task 1: vole-kit 0.7.0 – push-kanal, enhetsregister, Pigeon-klient

**Repo:** `Vigilant-Vole`, crate `kit/`.

**Filer:**
- Opprett `kit/src/push/` med:
  - `relay.rs`: Pigeon-klient, PoW, registrering, sending;
  - `devices.rs`: enhetsregisteret;
  - `events.rs`: lagring av `ref`-tekst;
  - `api.rs`: rutene.
- Opprett leverandøren `kit/src/notify/providers/hedgerow.rs`. Ny `Transport::Relay` i `schema.rs`, håndtert i `notify::send`.
- Opprett kjernemigrasjon `kit/migrations/0006_push.sql` med tabellene `push_devices` (id, user_id, token_id, handle_sealed, handle_hash, created_at, last_used_at) og `push_events` (ref, title, body, kind, level, created_at).
- Lokaler (26 språk) og dokumentasjon.

**Rutene:**
- `POST /api/v1/push/devices` med `{ "handle": "hrh_…" }` svarer `201 { "id": … }`. Samme handle to ganger gir `200` med samme id. Uten aktiv `hedgerow`-kanal svarer den `409 push_disabled`.
- `DELETE /api/v1/push/devices/{id}`: bare egne enheter. Den svarer `204`, og `404` for andres.
- `GET /api/v1/push/events/{ref}` svarer `{ "title", "body", "kind", "level", "occurred_at" }`, eller `404` når den er utløpt eller ukjent.

**Steg:**
- [ ] Tester:
  - PoW-løser mot kjente vektorer;
  - registrering mot en falsk relay;
  - sending i porsjoner på 25;
  - `gone` sletter handle;
  - `unknown_instance` gir én ny registrering;
  - `suspended` stopper;
  - tidsgrensen;
  - lesetoken kan bruke de tre rutene og ingenting annet (Review Focus 1);
  - `push_events` utløper etter 24 t;
  - handles er forseglet i databasen;
  - `409` uten kanal.
- [ ] Implementer.
- [ ] Kjør hele kit-testpakken og VV-pakken på LANBox. Release-PR for `vole-kit 0.7.0`.

### Task 2: VV tar i bruk kit 0.7.0

**Repo:** `Vigilant-Vole` (rot-crate).

- [ ] Kanaltypen dukker opp i Varsler. Sjekk at monitorkobling og `is_default` virker for `hedgerow`, og at vedlikeholdsvarsler går dit.
- [ ] Ende-til-ende-test med falsk relay: monitor nede, så push med `ref`, så `GET /events/{ref}` gir «… er nede».
- [ ] Oppdater dokumentasjonen (README og nettsiden): «Varsler i Hedgerow-appen».
- [ ] Release VV.

### Task 3: KK og Wren på kit 0.7.0

**Repoer:** `Keen Kestrel`, `Wise-Wren`.

- [ ] Bump `vole-kit-v0.7.0` (avhengighet, build-dep og dev-dep).
- [ ] KK: `hedgerow`-kanalen fungerer i varselregler, og køen med nye forsøk gjelder.
- [ ] Wren: `broadcast` når `hedgerow`.
- [ ] Én test per produkt med falsk relay.
- [ ] Release begge.

### Task 4: Appen – handles per tilkobling og Notification Service Extension

**Repo:** `Hedgerow-App`.

**Filer:**
- HedgerowCore: `Clients/Pigeon.swift` får `createHandle` og `revokeHandle`. Ny `PushRoutes.swift` med server-API-et og `tag`-registeret.
- Appen: `PushController` (handles per tilkobling), Innstillinger (bryter per tilkobling).
- Nytt mål `HedgerowNotificationService` i `project.yml`. Det deler App Group og nøkkelring, og har `NotificationService.swift`.

**Steg:**
- [ ] Tester i core:
  - `tag`-registeret (lagre, slå opp, fjerne);
  - handle-livsløp: lage, registrere hos server, avregistrere og trekke tilbake;
  - feil: `409 push_disabled` gir status «Serveren har ikke slått på push», og `gone` gir ny handle;
  - parsing av `hr`-payload (v1, ukjent versjon, mangler felt);
  - `events/{ref}`-dekoding.
- [ ] NSE:
  - Les `hr.t`, slå opp tilkoblingen, hent `events/{ref}` med tokenet (10 s), og sett tittel og tekst.
  - Ved feil står den generelle teksten (Review Focus 4).
  - `threadIdentifier` er tilkoblingen.
- [ ] Trykk på varselet åpner riktig fane med riktig tilkobling (VV gir Monitorer, KK gir Feil, Wren gir Oppdateringer).
- [ ] CI grønn. Eieren tester på telefon.

### Task 5: Utrulling

- [ ] VPS: oppdater VV, KK og Wren til de nye versjonene.
- [ ] Eieren legger til «Hedgerow-appen»-kanalen og kobler den til en testmonitor. Appen slår på push for tilkoblingene.
- [ ] Ta ned en testmonitor. Varselet skal komme med ekte tekst.
