# Vigilant Vole design system

The contract for every page in the app. Pages are composed **only** from the
classes, macros and `data-*` hooks below. If something you need is missing, ask
the design owner — don't add CSS, colours or inline styles.

- Stylesheet: `static/style.css` (tokens on `:root`, one file, organised by section)
- Behaviour: `static/app.js` (vanilla, `defer`, progressive enhancement; API on `window.VV`)
- Icons: `static/icons.svg` (sprite) · Favicon/app mark: `static/favicon.svg` · Home-screen icons: `apple-touch-icon.png` (180, full-bleed), `icon-192.png`, `icon-512.png`, `icon-maskable-512.png`, linked with `site.webmanifest` from `layout.html` / `auth_layout.html` (regenerate the PNGs from the mark if it changes)
- Font: Inter variable, self-hosted in `static/fonts/` (OFL licence alongside), system-ui fallback
- Macros: `templates/macros.html`
- Layouts: `layout.html`, `settings_layout.html`, `auth_layout.html`, `public_layout.html`

The look: Apple-style and calm, matching sylobot.com. Dark first with an
equally finished light theme. Hairline borders, 18px card radius, pill buttons
that press to 0.97, glass top bar and tab bar, Inter with tight tracking on
headings. The brand purple (`--brand`) is for links, focus, selection and the
chart line only.

---

## 1. Rules (do / don't)

**Do**
- Extend `layout.html` for app pages and `settings_layout.html` for settings pages (Layout contract below).
- Import macros once per template: `{% import "macros.html" as m %}`.
- Use the status classes (`status-up` …) for anything status-coloured.
- Write copy in sentence case, address the user as "you", use numerals ("3 monitors"), curly quotes (“ ” ’) and the ellipsis character (…). Name buttons for what they do ("Save monitor", "Add channel", "Revoke"), never "Submit" or "OK".
- Give every icon-only button an `aria-label` (and `data-tip` for a tooltip). Decorative icons are `aria-hidden="true"` (the macro does this).
- Give every input a `<label>` (or `aria-label` in compact rows like the header editor).
- Make every page work at 320px wide: use `.list`, `.table-stack`, `.field-row`, `.grid-2`, which all collapse on their own.
- Use `hx-confirm` / `data-confirm` for destructive actions; it opens the confirm sheet.

**Don't**
- No `<style>` blocks, no `style="…"` attributes, no new colours, no hex values in templates. (One exception: none. Tag colours are palette class names.)
- No `alert()`, `confirm()`, `prompt()`.
- No inline `onclick=` / `hx-on` JavaScript; use the `data-*` hooks.
- Don't put monospace on data labels; use `.mono` only for code, tokens, hashes.
- Don't build ALL-CAPS labels or eyebrows above headings.
- Don't animate anything yourself. Motion lives in style.css/app.js and respects reduced motion.

**Status colour mapping** (one class sets `--status`, `--status-text`, `--status-fill`)

| Class | Meaning | Colour |
|---|---|---|
| `status-up`, `status-success` | up, resolved, success | green |
| `status-down`, `status-danger`, `status-error` | down, outage, error | red (only `down` dots pulse) |
| `status-pending`, `status-warning` | pending/retrying, degraded, warning | yellow |
| `status-maintenance`, `status-info` | maintenance, informational | blue |
| `status-paused`, `status-unknown`, `status-none` | paused, no data | gray |

Because `HeartbeatStatus` displays as `up`/`down`/`pending`/`maintenance`,
`class="status-{{ s }}"` works directly. Incident severity
(`info|warning|danger|success`) and flash kinds (`success|error|info`) map too.

---

## 2. Layout contract

Every authenticated full-page template struct has `ctx: crate::api::PageCtx`:

```rust
pub struct PageCtx {
    pub title: String,          // <title>{title} · {site_name}</title>
    pub nav: &'static str,      // "dashboard" | "status-pages" | "maintenance" | "settings"
    pub subnav: &'static str,   // "general"|"notifications"|"users"|"security"|"tokens"|"agents"|"integrations"|"backup"|""
    pub user: Option<UserView>, // { id, username, display_name, role: Role, initials }
    pub site_name: String,
    pub flash: Option<Flash>,   // { kind: "success"|"error"|"info", message } → shown as a toast
    pub lang: String,           // <html lang>
}
// helpers usable in templates: ctx.can_edit(), ctx.is_admin(), u.role.label()
```

Build it with `PageCtx::build(&state, Some(&user), &jar, "Title", "nav", "subnav").await`
and return the jar so the flash cookie is cleared. Set a flash for the next page
with `crate::api::flash(jar, "success", "Monitor saved")` before a redirect.

| Layout | Fields | Blocks |
|---|---|---|
| `layout.html` | `ctx` | `head`, `main_class`, `content`, `scripts` |
| `settings_layout.html` (extends layout) | `ctx` (with `subnav`) | `settings_content` (+ `head`, `scripts`) |
| `auth_layout.html` | `title: String`, `error: Option<String>` | `subtitle`, `content`, `head`, `scripts` |
| `public_layout.html` | `page_title: String`, `theme: String` ("auto"/"light"/"dark"), `custom_css: Option<String>` | `header`, `content`, `footer`, `head`, `scripts` |

- The app shell gives you: glass top bar (brand, Dashboard / Status pages /
  Maintenance / Settings, account menu with Security, API tokens, API docs,
  Appearance Auto/Light/Dark, Log out), a bottom tab bar on phones, the toaster
  (renders `ctx.flash`), the request progress bar and a skip link.
- `<main class="main container">` is a flex column with a 32px gap (24px on
  phones): just stack your top-level blocks inside `{% block content %}`; no wrapper needed.
- Settings sidebar visibility: General, Users, Remote agents, Integrations,
  Backup → admins; Notifications → editors and admins (`ctx.can_edit()`);
  Security, API tokens → everyone. Settings pages still enforce roles server-side.
- Navigation links in the top bar, tab bar and settings sidebar are
  `hx-boost`ed (body swap + view transition). Boosted pages don't re-run
  `{% block head %}`, so don't rely on it for anything but extra meta tags.
- `public_layout` puts `custom_css` in a `<style>` with `|safe`. The status-pages
  owner must reject or strip `</style` on save.

---

## 3. Page skeletons (copy-paste)

### List page

```html
{% extends "layout.html" %}
{% import "macros.html" as m %}

{% block content %}
{% call m::page_header("Status pages", "Public pages that show how your services are doing.") %}
  {%- if ctx.can_edit() %}{{ m::link_button("/status-pages/new", "New status page", "primary", "plus") }}{% endif %}
{% endcall %}

{% if pages.is_empty() -%}
<div class="empty-card">
  {{ m::empty_state("globe", "No status pages yet", "Share uptime with your users on a page you control.", "/status-pages/new", "New status page") }}
</div>
{%- else -%}
<ul class="list" role="list">
  {%- for p in pages %}
  <li>
    <a class="list-row" href="/status-pages/{{ p.id }}/edit">
      <span class="list-icon">{{ m::icon("globe") }}</span>
      <span class="list-main">
        <span class="list-title">{{ p.title }}</span>
        <span class="list-sub">/status/{{ p.slug }}</span>
      </span>
      <span class="list-trail">{% if !p.published %}<span class="pill status-paused">Draft</span>{% endif %}{{ m::icon("chevron-right", "list-chevron") }}</span>
    </a>
  </li>
  {%- endfor %}
</ul>
{%- endif %}
{% endblock %}
```

### Form page

```html
{% extends "layout.html" %}
{% import "macros.html" as m %}

{% block content %}
{% call m::page_header("New maintenance window", "", "/maintenance", "Maintenance") %}{% endcall %}

<form class="form form-narrow" method="post" action="/maintenance/new" data-warn-unsaved>
  <section class="form-section" aria-labelledby="sec-basics">
    <h2 class="form-section-title" id="sec-basics">Basics</h2>
    <div class="card">
      {#- title_error: String, empty when valid -#}
      {{ m::field_text("title", "Title", value=&form.title, placeholder="Database upgrade…", required=true, error=&form.title_error) }}
      {{ m::field_textarea("description", "Description", &form.description, 3) }}
      <div class="field">
        <span class="label" id="strategy-label">Schedule</span>
        <div class="segmented segmented-block" role="radiogroup" aria-labelledby="strategy-label">
          <label><input type="radio" name="strategy" value="manual" checked><span>Manual</span></label>
          <label><input type="radio" name="strategy" value="once"><span>Once</span></label>
          <label><input type="radio" name="strategy" value="weekly"><span>Weekly</span></label>
        </div>
      </div>
      <div class="field-row" data-show-when="strategy=once,weekly">
        <div class="field"><label class="label" for="f-start">Starts</label><input id="f-start" name="start_at" type="datetime-local"></div>
        <div class="field"><label class="label" for="f-end">Ends</label><input id="f-end" name="end_at" type="datetime-local"></div>
      </div>
    </div>
  </section>

  <section class="form-section" aria-labelledby="sec-alerts">
    <h2 class="form-section-title" id="sec-alerts">Alerts</h2>
    <div class="card">
      {{ m::switch("active", "Active", form.active, "Monitors in this window don't send alerts") }}
      <details class="disclosure">
        <summary>Advanced <span class="disclosure-sub">Time zone, status pages</span>{{ m::icon("chevron-down") }}</summary>
        <div class="disclosure-body">
          …more fields…
        </div>
      </details>
    </div>
  </section>

  <div class="form-actions">
    {{ m::link_button("/maintenance", "Cancel", "secondary") }}
    {{ m::button("Save window") }}
  </div>
</form>
{% endblock %}
```

### Detail page

```html
{% extends "layout.html" %}
{% import "macros.html" as m %}

{% block content %}
<header class="page-head">
  <a class="page-back" href="/">{{ m::icon("chevron-left") }}Dashboard</a>
  <div class="page-head-row">
    <div class="page-head-text">
      <div class="page-title-group">
        <h1 class="page-title">{{ monitor.name }}</h1>
        {{ m::status_pill_opt(status, "lg") }}
      </div>
      <p class="page-sub">{{ monitor.monitor_type.label() }} · <span class="break">{{ monitor.target }}</span></p>
    </div>
    {%- if ctx.can_edit() %}
    <div class="page-actions">
      <a class="btn btn-secondary" href="/monitors/{{ monitor.id }}/edit">{{ m::icon("pencil") }}<span>Edit</span></a>
      <button type="button" class="btn btn-danger" hx-delete="/monitors/{{ monitor.id }}" hx-swap="none"
        hx-confirm="Delete “{{ monitor.name }}”?" data-confirm-message="Its check history goes with it."
        data-redirect-after="/">{{ m::icon("trash") }}<span>Delete</span></button>
    </div>
    {%- endif %}
  </div>
</header>

<div class="stats">…stat tiles…</div>

<div class="grid-2">
  <section class="card" aria-labelledby="cfg">
    <div class="card-head"><h2 class="card-title" id="cfg">Configuration</h2></div>
    <dl class="dl">
      <div class="dl-row"><dt>Check every</dt><dd class="num">{{ monitor.interval_seconds }} sec</dd></div>
    </dl>
  </section>
  <section class="card" aria-labelledby="ev">…timeline…</section>
</div>
{% endblock %}
```

### Settings page

```html
{% extends "settings_layout.html" %}
{% import "macros.html" as m %}

{% block settings_content %}
{% call m::page_header("API tokens", "Let scripts and dashboards read or change your monitors.") %}
  <button type="button" class="btn btn-primary" data-sheet-open="#new-token" commandfor="new-token" command="show-modal">{{ m::icon("plus") }}<span>New token</span></button>
{% endcall %}

<section class="section" aria-labelledby="tokens-title">
  {% call m::section_head("Your tokens", "", "tokens-title") %}{% endcall %}
  <div class="card card-flush">
    <div class="table-wrap">
      <table class="table table-stack">
        <thead><tr><th>Name</th><th>Scope</th><th>Last used</th><th><span class="vh">Actions</span></th></tr></thead>
        <tbody>
          {%- for t in tokens %}
          <tr id="token-{{ t.id }}">
            <td class="td-primary">{{ t.name }}</td>
            <td data-label="Scope"><span class="badge">{{ t.scope }}</span></td>
            <td data-label="Last used">{% match t.last_used_at %}{% when Some(at) %}{{ m::time_ago(at) }}{% when None %}Never{% endmatch %}</td>
            <td class="td-actions">
              <button type="button" class="btn btn-danger btn-sm" hx-delete="/settings/tokens/{{ t.id }}"
                hx-target="#token-{{ t.id }}" hx-swap="outerHTML" hx-confirm="Revoke “{{ t.name }}”?"
                data-confirm-label="Revoke" data-confirm-message="Anything using it stops working immediately.">Revoke</button>
            </td>
          </tr>
          {%- endfor %}
        </tbody>
      </table>
    </div>
  </div>
  <p class="section-foot">Tokens act as you, with the scope you choose.</p>
</section>
{% endblock %}
```

### Empty state

```html
<div class="empty-card">
  {{ m::empty_state("wrench", "No maintenance scheduled", "Plan downtime so alerts stay quiet while you work.", "/maintenance/new", "Schedule maintenance") }}
</div>
```
Inside a card use `{{ m::empty_state(…) }}` without `.empty-card`. Need two buttons? Write it out:
`<div class="empty"><div class="empty-icon">{{ m::icon("bell") }}</div><p class="empty-title">…</p><p class="empty-text">…</p><div class="empty-actions">…buttons…</div></div>`.

### Sheet with a form (htmx submit, closes on success)

```html
<button type="button" class="btn btn-primary" data-sheet-open="#new-tag" commandfor="new-tag" command="show-modal">{{ m::icon("plus") }}<span>New tag</span></button>

<dialog class="sheet" id="new-tag" aria-labelledby="new-tag-title">
  <div class="sheet-grabber" aria-hidden="true"></div>
  <header class="sheet-head">
    <h2 class="sheet-title" id="new-tag-title">New tag</h2>
    <button type="button" class="btn btn-icon btn-plain btn-sm" data-sheet-close commandfor="new-tag" command="close" aria-label="Close">{{ m::icon("x") }}</button>
  </header>
  <div class="sheet-body">
    <form id="new-tag-form" class="form" hx-post="/tags" hx-target="#tag-list" hx-swap="beforeend">
      {{ m::field_text("name", "Name", placeholder="production…", required=true) }}
    </form>
  </div>
  <footer class="sheet-foot">
    <button type="button" class="btn btn-secondary" data-sheet-close commandfor="new-tag" command="close">Cancel</button>
    <button type="submit" class="btn btn-primary" form="new-tag-form">Create tag</button>
  </footer>
</dialog>
```
Server response on success: the new row HTML plus header
`HX-Trigger: {"vv:toast": {"message": "Tag created", "kind": "success"}, "vv:close-sheet": true}`.
(`commandfor/command` make it work without JS in browsers that support invoker commands; app.js takes over when present.)
Sizes: `.sheet` (35rem), `.sheet-lg` (48rem). On phones every sheet is a bottom sheet with drag-to-dismiss.
Add `data-sheet-static` to the `<dialog>` to stop scrim clicks from closing it.

### Confirm-delete with htmx

```html
{{ m::delete_button("/settings/notifications/3", "Delete “Ops Discord”?", "Monitors stop sending alerts to this channel.", "#channel-3", "outerHTML") }}
```
or by hand: any element with `hx-delete` + `hx-confirm="Question?"`. Optional:
`data-confirm-message` (detail line), `data-confirm-label` (button text, default
"Delete" for destructive, else "Confirm"), `data-confirm-cancel`,
`data-confirm-destructive="true|false"` (default: true for `hx-delete` or when the
question says delete/remove/revoke/reset/disconnect). Plain forms and links:
`<form method="post" data-confirm="Reset all settings?">` or `<a href data-confirm="…">`.
Use `data-redirect-after="/somewhere"` on the htmx element to navigate after
success (or send `HX-Redirect` from the handler — preferred).

---

## 4. Components

Classes are listed with the element they go on. Everything is responsive and
themed already.

### Text & utilities
- Headings: `h1` (32px, page titles), `h2` (20px), `h3` (17px), `h4` (15px semibold). Apple text styles as classes: `.t-large-title .t-title1 .t-title2 .t-title3 .t-headline .t-body .t-callout .t-footnote .t-caption`.
- `.muted`, `.text-ok`, `.text-bad`, `.text-warn`, `.text-status` (status text colour, first letter raised; pair with a `status-*` class), `.cap` (raise only the first letter of a lowercase machine value like `up` → “Up”; never use `text-transform: capitalize`, it title-cases every word and mangles user text), `.num` (tabular numerals — use on every number column), `.mono`, `.truncate`, `.break` (long URLs), `.nowrap`, `.ta-right`, `.ta-center`, `.prose`, `.vh` (visually hidden).
- Layout: `.stack` (column, 24px gap; `.stack-sm` 12, `.stack-xs` 4, `.stack-lg` 40 — the size classes also work alone, `class="card stack-sm"` is a 12px column), `.cluster` (wrapping row, 8px gap; `.cluster-between`; inputs, selects, `.input-group` and `.search` inside share the row with `flex: 1 1 12rem` instead of taking 100%), `.grid-2`, `.grid-3` (auto-collapsing; cells keep their own height), `.split` (2:1, stacks under 1000px), `.grow`, `.container`, `.container-narrow`, `.hide-sm` / `.show-sm` (720px).

### Page header
`header.page-head` > optional `a.page-back` (chevron + label) > `.page-head-row` > `.page-head-text` (`h1.page-title`, optional `.page-title-group` to put a pill next to it, `p.page-sub`, optional `.tags`) + `.page-actions`.
Single-button `<form>`s inside `.page-actions` lay out like plain buttons (`display: contents`). On phones the actions fill the width; add `.btn-collapse` (+ `aria-label`) to a labelled button to make it icon-only under 560px when four actions would not fit (monitor detail “Check now”).
Macro: `{% call m::page_header(title, subtitle="", back_href="", back_label="Back") %}actions{% endcall %}`.

### Sections
`.section` (column, 12px gap) > `.section-head` (`h2.section-title`, `.section-sub`, `.toolbar` for actions) > card/list > `p.section-foot` (help under a group).
Macro: `{% call m::section_head(title, subtitle="", id="") %}actions{% endcall %}`.

### Buttons
`.btn` + one of `.btn-primary` (inverted pill, one per view), `.btn-secondary` (gray fill), `.btn-ghost` (outline), `.btn-plain` (brand text), `.btn-danger` (tinted red), `.btn-danger-solid` (confirm only). Sizes `.btn-sm`, `.btn-lg`, `.btn-block`; icon-only `.btn-icon` (+ `.btn-plain` for a quiet toolbar icon). Put labels in `<span>` next to an icon. Group: `.btn-group` (segmented-looking attached buttons), `.toolbar` (wrapping row).
`.btn-collapse` = icon-only on phones (keeps its label on wider screens; needs `aria-label`).
Disabled: `disabled` / `aria-disabled="true"`. Loading: htmx adds `.htmx-request` → spinner replaces the label; plain form posts get `.is-submitting` from app.js (double submits are blocked).
Macros: `m::button(label, kind="primary", icon_name="", type="submit", size="")`, `m::link_button(href, label, kind="secondary", icon_name="", size="")`, `m::icon_button(icon_name, label, size="")`.
Touch screens get ≥44px targets automatically.

### Cards
`.card` (padding 20) · `.card-flush` (no padding; for tables/lists/empty states) · `.card-head` (`h2.card-title`, `.card-sub`, `.toolbar`) · `.card-body` · `.card-foot` (right-aligned actions over a hairline).
Macro: `{% call m::card_head(title, subtitle="") %}actions{% endcall %}`.

### Inset grouped list (iOS Settings rows)
```html
<ul class="list" role="list">
  <li><a class="list-row" href="…">
    <span class="list-icon">{{ m::icon("bell") }}</span>          <!-- optional; add status-* to tint -->
    <span class="list-main"><span class="list-title">Title</span><span class="list-sub">Subtitle</span></span>
    <span class="list-trail">Value{{ m::icon("chevron-right", "list-chevron") }}</span>
  </a></li>
  <li class="list-row">{{ m::switch("flap_detection", "Flap detection", true) }}</li>
</ul>
```
- Rows can be `li.list-row`, `li > a.list-row`, or `label.list-row`. Separators inset to the text automatically (after an icon, provider tile, dot or drag handle).
- `.list-title` truncates to one line. For user-typed names that may only differ at the end (monitor names, maintenance titles) add `.list-title-wrap`: wraps and clamps at 2 lines (3 on phones).
- A pill that would squeeze the title on phones: render it twice — `<span class="show-sm">pill</span>` as the last child of `.list-main` and `class="pill … hide-sm"` in `.list-trail` (maintenance list).
- Whole-row link with buttons inside: put `class="stretched"` on the title link and actions in `.list-actions`.
- Numeric columns: `.list-value` (`<strong>` + `<small>` caption). Heartbeat strip: `.list-beats` (drops to its own line on phones).
- Extra full-width content (an uptime bar): `.list-row.list-row-wrap` + child `.list-full`.
- `.list-empty` for a "nothing here" row. Inside a `.card` the list loses its own chrome.

### Definition list
`dl.dl > div.dl-row > dt + dd` (label left, value right, hairlines between). `dl.dl.dl-stack`: values that are sentences go under their label on phones.

### Status
- Pill: `{{ m::status_pill(s) }}` / `{{ m::status_pill_opt(opt_status, "lg") }}` → `<span class="pill status-up"><span class="dot"></span><span class="cap">up</span></span>` (first letter raised by `.cap`). Custom text is shown exactly as written, so write it in sentence case: `<span class="pill status-paused">Waiting for first sync</span>`.
- Dot: `{{ m::status_dot(s) }}`, `{{ m::status_dot_opt(opt) }}` → `.dot.status-*` with `role="img"` + label. Only `down` pulses (off under reduced motion). `.dot-lg`.
- Badge / counter: `.badge` (gray), `.badge-brand`, `.badge.status-*` (tinted), `.badge.badge-solid.status-down` (iOS red counter).
- Stat tiles: `.stats` grid > `.stat` (`.stat-label` with optional `.stat-dot`, `.stat-value` (+`<small>` unit), `.stat-meta`). Add `status-*` on `.stat` to colour the dot. `a.stat` is pressable. Macro `m::stat(label, value, status="", meta="")`.
- Heartbeat bar: `{{ m::heartbeat_bar(&row.recent, "Recent checks", size="") }}` for an oldest-first `Vec<Option<HeartbeatStatus>>`; raw: `.beats > .beat.status-up` (`None` → `.beat` gray). `.beats-sm` for compact. Newest is on the right; narrow screens clip the oldest.
- Uptime bar (status pages, 90 days):
  ```html
  <div class="uptime">
    <div class="uptime-days" role="img" aria-label="90-day uptime 99.98%">
      <span class="uptime-day status-up" data-tip="Oct 3 · 100% uptime"></span>  <!-- status-down / status-pending (partial) / status-maintenance / status-none -->
    </div>
    <div class="uptime-legend"><span class="hide-sm">90 days ago</span><span class="show-sm">45 days ago</span><span>99.98%</span><span>Today</span></div>
  </div>
  ```
  Phones show the newest 45 days (CSS). Tooltips come from `data-tip` (tap on touch).
- Overall banner: `.banner.status-up > .banner-icon (icon check|x|alert|wrench) + div (p.banner-title, p.banner-sub)`.
- Incident card: `article.incident.status-{severity} > .incident-head ({{ icon }}, h3.incident-title, p.incident-meta) + p.incident-body + .incident-actions`. Icons: danger `alert-circle`, warning `alert`, info `info`, success `check-circle`.
- Callout (inline notice): `{{ m::callout("warning", "Copy this token now. You won't see it again.", "Shown once") }}` — kinds info|success|warning|danger (danger gets `role="alert"`).
- Timeline: `ol.timeline > li.timeline-item.status-* > span.timeline-dot + div (.timeline-head (.timeline-title, .timeline-time), p.timeline-text)`.
- Progress ring: `<span class="ring-wrap"><svg class="ring status-up" viewBox="0 0 36 36" role="img" aria-label="99.9% uptime"><circle class="ring-track" cx="18" cy="18" r="15.5"/><circle class="ring-value" cx="18" cy="18" r="15.5" pathLength="100" stroke-dasharray="99.9 100"/></svg><span class="ring-label">99.9%</span></span>` (`.ring-lg` 72px).
- Live (SSE) indicator: `{{ m::live() }}` — app.js switches Connecting… → Live / Reconnecting… / Offline.

### Charts (SVG)
```html
<div class="chart">
  <svg viewBox="0 0 600 120" preserveAspectRatio="none" role="img" aria-label="Response time, peak 312 ms">
    <line class="chart-grid" x1="0" x2="600" y1="60" y2="60"></line>
    <rect class="chart-band" x="350" y="0" width="40" height="120"></rect>       <!-- downtime; add status-maintenance for blue -->
    <polygon class="chart-area" points="…line points… 600,120 0,120"></polygon>  <!-- optional fill -->
    <polyline class="chart-line" points="…"></polyline>
  </svg>
  <div class="chart-scale" aria-hidden="true"><span>312 ms</span><span>0 ms</span></div>
</div>
<div class="chart-axis"><span>24 h ago</span><span>Now</span></div>
```
Strokes use `vector-effect: non-scaling-stroke`, so stretching the viewBox is fine. Inline sparkline: `<svg class="sparkline" …><polyline class="chart-line" …/></svg>`. Legend: `.legend > .legend-item > .legend-swatch.status-*`. The line is brand purple by design; downtime and maintenance are shown with `.chart-band`, not by recolouring the line.

### Tables
`.table-wrap > table.table` (+ `.table-hover`). Number cells `.num` (right-aligned, tabular), primary cell `.td-primary`, action cell `.td-actions`. Add `.table-stack` and `data-label="Column"` on each `td` so it becomes stacked cards under 640px; give sentence-length cells `.td-block` so the text sits under its label, left-aligned. Wrap in `.card.card-flush`.

### Forms
- Containers: `form.form` (column, 24px gap; `.form-narrow` = 40rem) > `section.form-section` (`h2.form-section-title`, `p.form-section-sub`, then a `.card` — fields inside are spaced automatically) > `.form-actions` (Cancel, then primary on the right; full width on phones).
- Field: `.field > label.label[for] + control + p.help + p.field-error`. Optional marker: `<span class="label-optional">Optional</span>` inside the label. Side by side: `.field-row` (auto-collapses).
- Text inputs, selects and textareas are styled by element — no class needed. 16px font (no iOS zoom), 44px tall, brand focus ring.
- Invalid: `aria-invalid="true"` on the control (or `.field.is-invalid`), message in `.field-error` with `{{ m::icon("alert-circle") }}`; the browser's `:user-invalid` is styled too. app.js focuses the first `aria-invalid` field on load. Error text says how to fix it ("Enter a full URL, like https://example.com").
- Unit suffix: `.input-group > input + span.input-addon` ("sec", "ms", "days"); prefix: `span.input-addon` first.
- Code: `textarea.textarea-code`.
- Checkbox: `{{ m::checkbox(name, label, checked, help="") }}` → `label.check`. Switch: `{{ m::switch(name, label, checked, help="") }}` → `label.switch-row` with `span.switch > input.switch-input[role=switch] + span.switch-ui`. Both submit `name=true` when on and **nothing** when off → `#[serde(default)] name: bool` in Rust.
- Radio cards: `.choices > label.choice > input[type=radio] + span.choice-body ({{ icon }}, .choice-title, .choice-sub)`.
- Segmented control (2–5 options): `.segmented[role=radiogroup][aria-label] > label > input[type=radio] + span`. Also links (`a[aria-current="page"]`) or buttons (`[aria-pressed]`). `.segmented-block` = full width. app.js slides the thumb.
- Chips (filters): `.chips` (`.chips-scroll` = single scrolling row on phones) > `button.chip[aria-pressed]` / `a.chip[aria-current]` / `label.chip > input[type=checkbox|radio]`. May contain `.dot.status-*` and `.badge`.
- Search: `<label class="search">{{ m::icon("search") }}<input type="search" placeholder="Search monitors…" aria-label="Search monitors"></label>` (44px, same height as every other field).
- Password with a strength hint (advisory; the server enforces the rules):
  ```html
  <div class="field">
    <label class="label" for="f-password">Password</label>
    <input id="f-password" name="password" type="password" minlength="10" autocomplete="new-password" data-password-strength aria-describedby="f-password-help">
    <div class="meter" data-strength-meter aria-hidden="true"></div>
    <p class="help" id="f-password-help" data-strength-label aria-live="polite">At least 10 characters.</p>
  </div>
  ```
  app.js scores length, character variety and common words/sequences/repeats (0–4), fills the four-segment `.meter` and rewrites the help line (“Strength: Fair — avoid common words and sequences.”); it restores the original text when the field is emptied. `minlength` sets the “too short” bar.
- Browser time zone: `<input type="hidden" name="timezone" value="UTC" data-detect-timezone>` (or a `<select>`) gets the browser’s zone when the value is empty or `UTC` (selects only if that zone is an option); `[data-timezone-label]` elements in the same form show it.
- QR code: `<div class="qr"><img src="…" width="200" height="200" alt="…"></div>` — dark modules on a white card in both themes.
- Disclosure ("Advanced"): `details.disclosure > summary (text, span.disclosure-sub, {{ m::icon("chevron-down") }}) + div.disclosure-body`.
- Tags: `{{ m::tag(&t.name, &t.color, value="") }}` → `.tag.tag-{color}`; `.tags` wraps a set; `.tag-remove` button inside for editors; `.tag-swatch` for colour pickers. **Colour values are palette names**: `red orange yellow green mint teal cyan blue indigo purple pink brown gray` — store these in `tags.color` (map imported hex values to the nearest name).
- Key–value editor (headers): see `data-kv` below.
- Hidden, type-specific fields: `data-show-when` (below) or swap a partial with htmx (`hx-get="/monitors/form-fields?type=…" hx-trigger="change" hx-target="#type-fields"`).
- Placeholders show an example and end with "…". Set `autocomplete` deliberately (`field_text` defaults to `off`; pass `username`, `current-password`, `new-password`, `email` on auth forms). URLs/emails/usernames get `autocapitalize="none" spellcheck="false"`.
- `<form data-warn-unsaved>` asks before leaving with unsaved edits.

### Menus & popovers
Native popover API; app.js positions them next to the trigger, adds arrow-key navigation and closes on selection.
```html
<button type="button" class="btn btn-icon btn-plain" popovertarget="row-menu-7" aria-label="More actions for Payments API">{{ m::icon("more") }}</button>
<div class="menu" id="row-menu-7" popover>
  <a class="menu-item" href="/monitors/7/edit">{{ m::icon("pencil") }}Edit</a>
  <button class="menu-item" type="button" hx-post="/monitors/7/pause">{{ m::icon("pause") }}Pause</button>
  <hr class="menu-sep">
  <button class="menu-item menu-item-danger" type="button" hx-delete="/monitors/7" hx-confirm="Delete “Payments API”?">{{ m::icon("trash") }}Delete</button>
</div>
```
`.menu-label` (small heading), `[aria-checked="true"]` shows a check on a `.menu-item`, `data-keep-open` keeps the menu open after a click. `.popover` is the same surface with padding for small forms/filters. IDs must be unique per page (include the row id).

### Tabs
In-page: `<div data-tabs><div class="tabs" role="tablist" aria-label="…"><button class="tab" role="tab" id="t-a" aria-controls="p-a" aria-selected="true">…</button>…</div><div class="tab-panel" role="tabpanel" id="p-a" aria-labelledby="t-a">…</div>…</div>` (arrow keys work).
Page-level (state in the URL — preferred): `<nav class="tabs"><a class="tab" href="?tab=events" aria-current="page">Events</a>…</nav>`.

### Sheets, confirm, toasts
- Sheets: `dialog.sheet` (see skeleton). Hooks: `data-sheet-open="#id"`, `data-sheet-close`, `VV.openSheet(el|"#id", trigger)`, `VV.closeSheet(el?)`, events `vv:sheet-open` / `vv:sheet-close` (bubble from the dialog). Focus moves in (`autofocus` respected), is trapped (modal), and returns to the trigger.
- Confirm: `await VV.confirm({ title, message, confirmLabel, cancelLabel, destructive })` → `true|false`. Auto-wired for `hx-confirm` and `data-confirm`.
- Toasts: `VV.toast("Monitor saved", "success" | "error" | "info" | "warning")`; from the server: `HX-Trigger: {"vv:toast": {"message": "Saved", "kind": "success"}}`; after a redirect: `flash(jar, "success", "…")`. Toasts drop from the top, stack (max 3), pause on hover, swipe up or tap to dismiss. Errors stay 6.5s.

### Loading & empty
- Skeletons: `{{ m::skeleton_rows(3) }}` or `.skeleton` + `.skeleton-title` / `.skeleton-text` / `.skeleton-circle` / `.skeleton-block`, `.skeleton-row` (aria-hidden). Typical lazy section: `<div hx-get="/x/partial" hx-trigger="load" hx-swap="outerHTML">{{ m::skeleton_rows(3) }}</div>`.
- Region that dims while refreshing: add `.loading-region` to the target and point `hx-indicator` at it (or put it on the requesting element).
- `.htmx-indicator` (our styles; htmx's own are disabled), `.spinner` / `.spinner-lg`.
- A thin brand progress bar appears automatically when any htmx request takes over 150ms (`data-no-progress` on an element opts its requests out — use it for polling).
- Empty states: `.empty` (`.empty-icon`, `.empty-title`, `.empty-text`, `.empty-actions`), `.empty-card` = dashed frame for a page-level empty.

### Provider tiles (notification services)
`<span class="provider-tile provider-{kind}" aria-hidden="true">{{ m::icon(p.icon) }}</span>` — a 32px coloured tile. Brand services show a neutral monogram (D, S, Mm, Pd …; no logos), protocols (email, webhook, Telegram, Signal, Home Assistant) show the glyph you pass. Works in `.choice-body` (provider picker) and as the first child of a `.list-row` (separator insets like `.list-icon`). Kinds: discord slack teams googlechat mattermost rocketchat matrix webex feishu dingtalk telegram signal twilio ntfy pushover gotify pushbullet bark email pagerduty opsgenie webhook apprise homeassistant; unknown kinds fall back to a gray tile with the glyph.

### Server error pages
`api/error.rs` renders bare 404/500 pages as `body.error-page > main.container > .card.empty-state > h1 + p + p > a.btn`; style.css turns that into a centred card with the app mark, so keep that markup if it changes.

### Misc
- Copy field (shown-once tokens): `{{ m::copy_field("new-token", &token) }}`; any button: `data-copy="#element-id"` (copies its value/text) or `data-copy-text="literal"`; label flips to "Copied" (override with `data-copied-label`).
- Code: inline `<code>`, block `<pre class="code"><code>…</code></pre>`. Keys: `<kbd>`.
- Avatar: `{{ m::avatar(&u.initials, "sm"|"lg") }}` → `.avatar`.
- Relative time: `{{ m::time_ago(&row.checked_at) }}` → `<time datetime="2026-10-19 14:03:00" data-relative>` ("3 minutes ago", refreshed every 30s; DB UTC format, zone assumed UTC; future times read "in 3 days").
- Tooltip: `data-tip="Text"` on any element (hover, focus, or tap on touch).
- Sortable list (status page sections, monitor order):
  ```html
  <ul class="list" role="list" data-sortable data-sortable-group="sp-monitors" data-sortable-name="section_4[]">
    <li class="list-row" data-sortable-item data-id="12">
      <button type="button" class="drag-handle" data-sortable-handle aria-label="Reorder Website. Use Alt and the arrow keys.">{{ m::icon("grip") }}</button>
      <span class="list-main"><span class="list-title">Website</span></span>
    </li>
  </ul>
  ```
  Drag the handle (1:1, spring settle) or press Alt+↑/↓ on it. app.js keeps a hidden `<input name="{data-sortable-name or order[]}" value="{data-id}">` in each item in DOM order (items dragged into another list in the same `data-sortable-group` take that list's name) and fires a bubbling `vv:reorder` event (`detail.ids`). Auto-save: `<form hx-post="/status-pages/4/order" hx-trigger="vv:reorder" hx-swap="none">…list…</form>`. Repeated field names need `axum_extra::extract::Form` (serde_html_form) on the Rust side.

---

## 5. `data-*` hooks (app.js)

| Hook | Effect |
|---|---|
| `hx-confirm="Question?"` (+ `data-confirm-message/-label/-cancel/-destructive`) | Confirm sheet before the htmx request |
| `data-confirm="Question?"` on `form`, `a`, `button` | Confirm sheet before submit/navigation |
| `data-redirect-after="/path"` | Navigate after a successful htmx request |
| `data-sheet-open="#id"` / `data-sheet-close` / `data-sheet-static` | Open/close sheets; static = scrim click doesn't close |
| `popovertarget="id"` + `.menu[popover]` | Menu positioned at trigger, arrow keys, closes on choose; `data-keep-open` on an item |
| `data-tabs` + `role=tab/tabpanel` | In-page tabs |
| `data-show-when="field=a,b"` / `"field!=a"` | Show the element only for those values of the named control in the same form; hidden controls are disabled (not submitted, not validated) |
| `data-kv` (+ `data-kv-rows`, `template[data-kv-template]`, `data-kv-add`, `data-kv-remove`, optional `data-kv-json="#hidden"`) | Key–value row editor. With `data-kv-json` the rows are serialised as a JSON object into the hidden input (give it `disabled` in markup so it only submits when JS runs, and add a `<noscript><textarea name="…">` fallback if needed). Without it, give the row inputs names like `header_name[]`. |
| `data-copy="#id"` / `data-copy-text="…"` (+ `data-copied-label`) | Copy to clipboard |
| `time[data-relative]` | Relative time |
| `data-tip="…"` | Tooltip |
| `data-sortable`, `data-sortable-item`, `data-sortable-handle`, `data-id`, `data-sortable-group`, `data-sortable-name`, `data-sortable-label` | Sortable lists, `vv:reorder` event |
| `data-theme-set="auto|light|dark"` | Theme switch (already in the account menu) |
| `form[data-warn-unsaved]` | Prompt before leaving with edits |
| `data-no-progress` | Don't show the global progress bar for requests from inside this element |
| `data-toast="kind"` | Server-rendered toast (the layout uses it for `ctx.flash`) |
| `input[data-password-strength]` + `[data-strength-meter]` + `[data-strength-label]` (same `.field`) | Password strength meter and hint |
| `data-detect-timezone` (+ `[data-timezone-label]`) | Fill a time zone field from the browser |

JS API: `VV.passwordScore(pw, minLength)` → `{score 0–4, hint}`, `VV.confirm`, `VV.toast`, `VV.announce(text)` (screen reader), `VV.openSheet`, `VV.closeSheet`, `VV.setTheme`, `VV.spring({from, to, velocity, damping=1, response=0.35, onUpdate, onComplete})`, `VV.init(root)` (runs automatically for htmx-swapped content).
Events to send from the server via `HX-Trigger`: `vv:toast` (`{message, kind}`), `vv:close-sheet`.

---

## 6. htmx patterns

- **Delete a row:** `hx-delete` + `hx-target="#row-id"` + `hx-swap="outerHTML"` + `hx-confirm`. Respond `200` with an empty body (the row disappears) and optionally `HX-Trigger: {"vv:toast": …}`.
- **Inline create/update in a sheet:** form with `hx-post`, respond with the new/updated fragment + `HX-Trigger: {"vv:toast": …, "vv:close-sheet": true}`.
- **Validation errors:** htmx doesn't swap 4xx responses. Either return `200` with the re-rendered form fragment (fields carrying `aria-invalid` + `.field-error`), or set `HX-Retarget`/`HX-Reswap`. For plain (non-htmx) forms, re-render the page with errors, or `flash(jar, "error", "…")` + redirect.
- **Full navigation after an action:** `HX-Redirect: /path` (or `data-redirect-after`).
- **Lazy sections:** `hx-get` + `hx-trigger="load"` with `m::skeleton_rows()` as placeholder.
- **Polling (public status pages):** `hx-get="/status/{slug}/fragment" hx-trigger="every 60s" hx-swap="outerHTML" data-no-progress`.
- **SSE:** `hx-ext="sse" sse-connect="/events" sse-swap="event-name"`. app.js defers swaps while a sheet, menu or drag is active and applies the latest one afterwards, so open UI is never yanked away. Keep stable `id`s on focusable controls inside swapped regions so focus survives.
- **Server 5xx** on an htmx request shows an error toast automatically; network failures too.

---

## 7. Macros reference

| Macro | Signature |
|---|---|
| icon | `m::icon(name, cls="")` |
| status_pill / status_pill_opt | `(status, size="")` — size `"lg"` |
| status_dot / status_dot_opt | `(status)` |
| heartbeat_bar | `(beats, label="Recent checks", size="")` — size `"sm"` |
| uptime | `(Option<f64>)` → "99.95%" or — |
| ms | `(Option<i64>)` → "120 ms" or — |
| time_ago | `(datetime)` |
| page_header (call) | `(title, subtitle="", back_href="", back_label="Back")` |
| section_head (call) | `(title, subtitle="", id="")` |
| card_head (call) | `(title, subtitle="")` |
| empty_state | `(icon_name, title, text, action_href="", action_label="")` |
| callout | `(kind, text, title="")` |
| field_text | `(name, label, value="", kind="text", placeholder="", help="", required=false, autocomplete="off", error="")` |
| field_number | `(name, label, value, unit="", min="", max="", step="", help="")` |
| field_textarea | `(name, label, value="", rows=4, placeholder="", help="", code=false)` |
| switch / checkbox | `(name, label, checked, help="")` |
| button | `(label, kind="primary", icon_name="", type="submit", size="")` |
| link_button | `(href, label, kind="secondary", icon_name="", size="")` |
| icon_button | `(icon_name, label, size="")` |
| delete_button | `(url, question, message="", target="", swap="", label="Delete", size="sm")` |
| avatar | `(initials, size="")` |
| copy_field | `(id, value, label="Copy")` |
| tag | `(name, color, value="")` |
| live | `()` |
| stat | `(label, value, status="", meta="")` |
| skeleton_rows | `(count=3)` |

Askama 0.16 notes (these bite):
1. Macros without a body are expressions: `{{ m::icon("gear") }}`. Macros with a body (`page_header`, `section_head`, `card_head`) must be called as `{% call m::page_header("Title") %}…{% endcall %}` — even when the body is empty.
2. Pass `String` fields by reference: `value=&monitor.name`, `m::avatar(&u.initials)`. `Copy` values (numbers, bools, `Option<f64>`, status enums) pass as is. Lists: `m::heartbeat_bar(&row.recent)`.
3. An argument that is a **bare identifier equal to a parameter name of that macro** resolves to the macro's own parameter. `m::field_text("name", "Name", value=name)` renders the literal `"name"`. Write `value=&self.name`.
4. `format!()` is not available in templates; build URLs inline (`href="/monitors/{{ id }}/edit"`) or pass a prepared string from Rust.
5. Don't name a loop variable `m` (it shadows the macro import).
6. Inside a template that extends a layout, import macros again at the top: `{% import "macros.html" as m %}`.

---

## 8. Icons

`<svg class="icon" aria-hidden="true"><use href="/static/icons.svg#NAME"></use></svg>` or `{{ m::icon("NAME") }}`. Sizes: default 1.25em, `.icon-sm` (1em), `.icon-lg` (24px), `.icon-xl` (32px). Drawn for this project on a 24px grid, 1.5px stroke, `currentColor`.

`dashboard pulse heart globe wrench gear bell user users lock shield shield-check key server plug archive plus minus pencil trash copy pause play chevron-right chevron-left chevron-down chevron-up chevron-up-down check x alert alert-circle info check-circle x-circle circle-dashed clock calendar search filter sliders sort tag link external refresh more grip menu logout sun moon theme-auto box dns bolt chart chart-bar eye eye-off megaphone code mail send download upload logo`

Suggested mapping: monitors `pulse`, status pages `globe`, maintenance `wrench`, settings `gear`/`sliders`, notifications `bell`, users `users`, security `shield`, tokens `key`, agents `server`, integrations `plug`, backup `archive`, incidents `megaphone`, Docker `box`, DNS `dns`, push `bolt`, API `code`, email `mail`, webhook `link`, Telegram/test send `send`, pending `circle-dashed`.

---

## 9. Tokens (for reference — use classes, not raw tokens, in templates)

- Surfaces: `--bg`, `--bg-2`, `--card`, `--card-raised`, `--field`, `--fill`, `--fill-2`, `--hover`, `--pressed`, `--line`, `--line-strong`; materials `--bar`, `--material`, `--scrim`.
- Text: `--text`, `--muted`, `--faint` (decorative only), `--placeholder`, `--on-inverse`. Brand: `--brand`, `--brand-fill`, `--brand-ring`.
- Status: `--ok|warn|bad|info|neutral` each with `-text` and `-fill`.
- Palette `--pal-red … --pal-gray` (the 13 tag colours; also used by provider tiles).
- Space `--sp-1…16` (4px steps, rem), radii `--r-xs 6 · sm 10 · md 14 · lg 18 · xl 22 · full`, z-index `--z-nav 50 · tabbar 60 · popover 70 · toast 100 · tooltip 110`.
- Motion: `--ease-out cubic-bezier(.23,1,.32,1)`, `--ease-in-out`, `--ease-drawer cubic-bezier(.32,.72,0,1)`, `--dur-press 120ms`, `--dur-ui 200ms`, `--dur-sheet 340ms`; springs are critically damped (damping 1, response 0.35s).
- Themes: `light-dark()` tokens; `<html data-theme="light|dark">` forces one (the account menu stores the choice in localStorage; `static/theme.js` applies it before paint). Honoured preferences: `prefers-reduced-motion` (moves become fades, no pulse/shimmer), `prefers-reduced-transparency` (solid chrome), `prefers-contrast: more` (stronger lines and text), `forced-colors`.

The kitchen-sink preview used while building this lives outside the repo; every snippet above is taken from markup that was rendered and checked at 375px and 1280px in both themes.

---

## 10. Translations

- Languages are a registry (`src/i18n/lang.rs`, `LANGS`): English (the source), Norwegian bokmål, German, French, Spanish, Italian, Dutch, Brazilian Portuguese, Polish, Swedish and Danish. Each has one entry (BCP 47 code, English and own name, direction, CLDR plural rule, number conventions, `beta`) and one `locales/<code>.toml`; adding a language touches nothing else. How to translate, edit or add a language: [translating.md](translating.md).
- Strings live in `locales/<code>.toml`. `cargo test --lib` fails on a missing/extra key, an empty value, mismatched `{placeholders}`, plural forms that don't match the language's CLDR categories, malformed `[fmt]` lists, a shipped language below 100 % coverage, or an unknown key used in a template or `crate::i18n::t("…")`. A key missing from a language falls back to English per key.
- Templates: `{{ tr("nav.dashboard") }}`, `{{ tr1("detail.delete_q", "name", monitor.name) }}`, `{{ tr2(…) }}`, plurals `{{ trn("count.monitors", rows.len()) }}` (`key.one` / `key.other`, plus `key.few` / `key.many` … where the language's rule needs them). Pass translations to macros as arguments: `m::page_header(tr("nav.users"), tr("users.sub"))`. Status words: `{{ status_text(s) }}`. Numbers: `{{ pct(opt_f64) }}`, `{{ millis(opt_i64) }}`, `{{ num(n) }}`, `{{ dur(secs) }}`. Every module with a template struct needs `use crate::i18n::Tr as _;`. Don't name a template variable `tr`. `<html lang>` and `<html dir>` come from `{{ lang_code() }}` / `{{ lang_dir() }}` (`ctx.lang` / `ctx.dir` in the app layout).
- Rust: `crate::i18n::t("key")`, `tf("key", &[("name", &value)])`, `tn("key", n)`; dates/numbers/durations via `crate::i18n::fmt_*` (nb: `19. okt. 2026, 14:05`, `99,95 %`, `12 345`; de: `19. Okt. 2026, 14:05`, `12.345`). Number separators and `%` spacing come from the registry; month and weekday names, date patterns, duration units and relative-time phrases from the locale file's `[fmt]` section. Flash and toast text must go through these.
- The request language is a task-local: site default (Settings → General) → user override (Settings → Security); public status pages use the visitor's `Accept-Language` (regions and reserves resolve: `pt` → `pt-BR`, `de-AT` → `de`, `nn`/`no` → `nb`). `/api/*`, badges and `/metrics` stay English. Code that renders outside the request future (SSE streams, spawned tasks) must capture `i18n::current()` and re-enter with `i18n::scope` / `sync_scope`; `AppState::with_conn` already carries it into DB closures.
- JavaScript: layouts embed `<script type="application/json" id="vv-i18n">` with the `js.*` keys; app.js reads them with `VV.t(key, englishFallback)` and formats relative times with `Intl.RelativeTimeFormat(document.documentElement.lang)`.
- Copy: sentence case where the language uses it, the language's own quotation marks (« » fr/es/it, „ “ de/pl, ” ” sv, » « da, “ ” nl/pt-BR/nb), informal or neutral address. Terminology per language is recorded in `locales/glossary/<code>.md` (Norwegian: monitor = overvåking, incident = driftsmelding, notification = varsel/varsling). Keep buttons, tabs and pills as short as the English: shorten the translation, never the design.
- Right-to-left readiness: the stylesheet uses logical properties (`margin-inline-start`, `inset-inline-end`, `text-align: start`) and a `--dir` variable for direction-dependent transforms. A declaration that must stay physical carries a trailing `/* rtl-ok: reason */` comment; a test rejects new physical `left`/`right` properties without it.
