# The dsh web UI, surface by surface — and how to build each one in sica-rust

`deepseek-harness` (`dsh`) ships a browser client (`packages/client/ui-*`)
whose look and interaction model are the target for the sica-rust frontend.
This guide is the UI companion to [harness-implementation-guide.md](harness-implementation-guide.md):
that document ported the *harness* (event log, tools, compaction, delegation);
this one ports the *shell the user sits in front of*.

For every surface it does three things:

1. states what dsh renders — concrete values (px, colours, strings, timings),
   read from the CSS modules and React components, not paraphrased;
2. states what sica-rust renders today (`crates/frontend`);
3. says how to build the dsh version in egui — the module it belongs in, the
   `App` state it reads, the protocol change (if any), and a rough size.

Sizes: **S** = an afternoon · **M** = a day or two, maybe a protocol bump ·
**L** = a week, FE + BE · **XL** = its own project.

> **Status.** Waves **UI-1, UI-2 and UI-3** are implemented, along with the
> UI-4 settings modal and session rows — everything in them that needs no
> protocol change. §0 below therefore describes the *old* frontend; the
> current one is the design system in `sica_core::theme` + `crates/frontend/src/ui/kit.rs`,
> the three-column shell in `ui/mod.rs`, the transcript in `ui/chat/`, and the
> settings modal in `ui/settings/`. What is still open is listed in §12's
> table after each wave row.

Two ground rules up front:

- **Style, not brand.** sica-rust keeps its own identity (the blade mark,
  the `sica` wordmark). What is ported is the visual system — tokens,
  typography, geometry, elevation — and the interaction model. Nothing below
  proposes the whale logo, the DeepSeek blue as *brand*, or the "Into the
  Unknown" headline.
- **The event log is already dsh-shaped.** Every dsh transcript surface is a
  projection of the same kinds of events sica-rust already persists
  (`sica_core::event`). Most of the work is on the FE; the protocol changes
  are additive and listed once in §11.

---

## 0. Where the frontend is today

`crates/frontend` is eframe/egui 0.28 with `egui_commonmark`. The shape,
from [ui/mod.rs](../crates/frontend/src/ui/mod.rs):

```
┌──────┬──────────────┬────────────────────────────────────────────┐
│ rail │ sessions     │ chat (or settings)                          │
│ 56px │ 220px, drag  │   protocol banner                           │
│      │              │   transcript (ScrollArea)                   │
│ chat │ ACTIVE  +New │   ...                                       │
│ cfg  │ row          │   approval / goal / jobs / plan+todo strips │
│      │ row          │   [📎] [ composer, r=14 ] [Send]            │
│ sica │              │                                             │
├──────┴──────────────┴────────────────────────────────────────────┤
│ status bar: BE · IPC · LLM · workspace · PERM · CTX 43% · TOK/S  │
└──────────────────────────────────────────────────────────────────┘
```

The relevant facts for a restyle (full inventory in the file comments of
the modules named):

| Area | Today | Where |
| --- | --- | --- |
| Palette | 14 tokens, two hand-tuned themes `paper` (warm light) / `iron` (warm dark), orange accent `#B14A1F` / `#D86A2A`, radius 2, no shadows | [sica-core/src/theme.rs](../crates/sica-core/src/theme.rs), `App::apply_visuals` in [app.rs](../crates/frontend/src/app.rs) |
| Type | IBM Plex Mono for *everything* proportional; Newsreader italic for display/headings; tracked-caps labels as the main label style | [ui/fonts.rs](../crates/frontend/src/ui/fonts.rs), [ui/widgets.rs](../crates/frontend/src/ui/widgets.rs) |
| Shell | fixed 56 px rail (Chat / Settings) + 220 px sessions panel; settings is a full view with three underlined tabs, not a modal; bottom status bar with hard-coded white/grey text | [ui/sidebar.rs](../crates/frontend/src/ui/sidebar.rs), [ui/sessions_panel.rs](../crates/frontend/src/ui/sessions_panel.rs), [ui/status_bar.rs](../crates/frontend/src/ui/status_bar.rs) |
| Transcript | user = right-aligned sunk box r=2; assistant = "ASSISTANT" caps + hairline + markdown + Copy; reasoning = italic serif with a blue rule; tool calls = one flat wrapped row of `RUN/OK/ERR name` chips, depth ignored; notices = centred caps line between rules | [ui/chat/messages.rs](../crates/frontend/src/ui/chat/messages.rs), [ui/chat/tool_chips.rs](../crates/frontend/src/ui/chat/tool_chips.rs) |
| Composer | attach button + 2–8 row `TextEdit` in a r=14 frame + Send/Stop; Enter sends/queues, Ctrl+Enter steers, Shift+Enter newline; `/` palette renders *inside* the bottom panel | [ui/chat/input_bar.rs](../crates/frontend/src/ui/chat/input_bar.rs), [ui/chat/slash_menu.rs](../crates/frontend/src/ui/chat/slash_menu.rs) |
| Control plane | approval strip, goal strip, jobs strip, PLAN ON/OFF + read-only todo glyph list, all stacked above the composer; ask-user is a centred `egui::Window` | [ui/chat/control.rs](../crates/frontend/src/ui/chat/control.rs) |
| Meter | `used / budget · CTX 43%` and `TOK/S` in the status bar; `TokenBreakdown` arrives on the wire and is never drawn | `status_bar::draw_context_meter` |
| Sessions | flat list by `created_at`, title only, hover `×` → armed Delete/Keep | `sessions_panel::draw_row` |
| Diagnostics | log panel only under Settings → Communication; `Event::LogLine.level` is dropped in `supervisor::forward_event`, so every BE line is `INF` | [ui/log_panel.rs](../crates/frontend/src/ui/log_panel.rs), [supervisor.rs](../crates/frontend/src/supervisor.rs) |
| Dead on the FE | `MessageDump.context_source` (populated by the BE, never read — the FE sniffs `<skill_content`), `TokenBreakdown`, `Request::InjectContext`, `ToolChip.depth/parent_id` for historical chips | `App::rebuild_turns` |

---

## 1. Design system

### 1.1 What dsh does

Token authority is `packages/client/ui-theme/src/styles/design-platform.css`.
Three layers: a **static scale** (`--dsw-static-*`) that is byte-identical in
light and dark (one exception, `neutral-bluish-60`), a **semantic alias
layer** (`--dsw-alias-*`, `--dsw-specific-*`) that flips per theme, and
components that consume aliases only — never a literal colour, never a theme
selector (`docs/web-styling.md`). That is the whole discipline: one static
palette, two alias maps.

**Static scale — the ramps that matter.** `neutral-bluish` is the UI ramp:

```
00 #FFFFFF  50 #F9FAFB  60 #F5F6F7  75 #F1F3F5  100 #EBEEF2  150 #E9ECF2
200 #E1E5EE  300 #CFD3D6  400 #ADB2B8  500 #979DA6  600 #81858C  700 #61666B
750 #43454A  800 #353638  850 #2C2C2E  875 #232324  900 #1B1B1C  950 #151517
1000 #0F1115
```

Accent ramp (`deepseek`): `50 rgb(237,243,254)` · `100 rgb(228,237,253)` ·
`200 rgb(211,226,255)` · `400 #679EFE` · `450 #5686FE` · `500 #4176E6` ·
`800 rgb(52,65,91)`. State ramps: green `500 rgb(34,197,94)` / `100 rgb(230,250,237)` /
`900 rgb(35,60,44)`; red `600 rgb(236,19,19)` / `400 rgb(242,90,90)`;
amber `500 rgb(245,158,11)` / `600 rgb(221,134,41)` / `100 rgb(254,245,231)` /
`900 rgb(39,36,31)`.

**Alias map** (light → dark), the subset a desktop client needs:

| Alias | Light | Dark | Used for |
| --- | --- | --- | --- |
| `bg-base` | `#FFFFFF` | `#151517` | window ground, transcript |
| `bg-layer-1/2/3` | `#FFFFFF` | `#232324` / `#2C2C2E` / `#353638` | cards, modal, menus (light has no layering — it is all white with hairlines) |
| `sidebar-fill` | `#F9FAFB` | `#1B1B1C` | left column |
| `sidebar-nav-item-hover` / `-active` | `#F1F3F5` / `#EBEEF2` | `#2C2C2E` / `#43454A` | settings nav, rows |
| `specific-tip` | `#F5F6F7` | `#353638` | dock cards (todo, queue) |
| `specific-bubble` | `rgb(237,243,254)` | `#2C2C2E` | user bubble |
| `specific-input-major` | `#FFFFFF` | `#2C2C2E` | composer card |
| `specific-menu` | `#FFFFFF` | `#353638` | menus, popovers |
| `markdown-code-block` | `#F9FAFB` | `#1B1B1C` | code, IN/OUT card, diff, injection body |
| `markdown-inline-code` | `#EBEEF2` | `#2C2C2E` | inline code |
| `label-primary` | `#0F1115` | `#F9FAFB` | body text |
| `label-secondary` | `#61666B` | `#CFD3D6` | row titles |
| `label-tertiary` | `#81858C` | `#ADB2B8` | summaries, timestamps |
| `label-caption` | `#ADB2B8` | `#81858C` | placeholders, gutter labels |
| `label-primary-dimmed` | `#151517` | `#EBEEF2` | compaction marker |
| `label-primary-inverted` | `#FFFFFF` | `#353638` | text on primary button |
| `border-l1/l2/l3/l4` | black @ `.04/.10/.12/.16` | white @ `.06/.12/.16/.20` | every hairline |
| `interactive-bg-hover` | `rgba(38,49,72,.06)` | `rgba(255,255,255,.08)` | hover wash |
| `interactive-bg-active` | `rgba(38,49,72,.10)` | `rgba(255,255,255,.14)` | pressed |
| `interactive-bg-hover-danger` | `rgba(236,19,19,.05)` | `rgba(242,90,90,.15)` | Reject hover |
| `button-primary-fill` / `-hover` | `#0F1115` / `#43454A` | `#F9FAFB` / `#EBEEF2` | **primary buttons are ink, not blue** |
| `button-info-fill` / `-hover` | `#4176E6` / `#679EFE` | `#679EFE` / `#4176E6` | the send circle |
| `button-elevated-fill` | `#FFFFFF` | `#43454A` | New Session |
| `button-floating-fill` | `#FFFFFF` | `#2C2C2E` | back-to-bottom |
| `state-business-primary` | `#4176E6` | `#679EFE` | active tab, caret, links, running dot, focus ring |
| `state-business-tertiary` | `rgb(228,237,253)` | `rgb(52,65,91)` | preview badge |
| `state-success-primary` / `-tertiary` | `rgb(34,197,94)` / `rgb(230,250,237)` | same / `rgb(35,60,44)` | done dot, diff `+` |
| `state-warn-primary` / `-label` / `-tertiary` | `rgb(245,158,11)` / `rgb(221,134,41)` / `rgb(254,245,231)` | same / same / `rgb(39,36,31)` | approval strip, plan chip, pending dot |
| `state-error-primary` | `rgb(236,19,19)` | `rgb(242,90,90)` | failed dot, diff `-`, turn error |
| `tooltip-bg` | `#2C2C2E` | `#43454A` | tooltips (text always white) |
| `toast-bg` / `button-contrast-fill` | `#353638` / `#61666B` | `#43454A` / `#F9FAFB` | toast |
| `scrollbar-bg-l1` / `-hover-l1` | `rgb(229,229,229)` / `rgb(212,212,212)` | `rgb(60,60,61)` / `rgb(84,85,87)` | 8 px thumbs, r=4 |

Two rules the CSS states repeatedly: **`brand-primary` resolves to ink**
(`#0F1115` / `#F9FAFB`), so anything that must stay blue in both themes uses
`state-business-primary`; and there is no `info` alias — "info" *is*
`business`.

**Typography.** UI face is the system sans (`-apple-system, 'Segoe UI', …`);
code face is `'SF Mono', 'JetBrains Mono', 'Fira Code', Consolas, …`. Weights
400 / 500 / 600 / 700 only. Fixed roles (size/line-height, weight):

```
xl-24  24/32 600     l-20 20/28 500     m-18 16/28 500
base-16 16/24 400    base-strong 16/24 500
s-14 14/22 400       s-strong 14/22 500
xs-13 13/20 400      xs-strong 13/20 500
xxs-12 12/18 400     xxs-strong 12/18 500
xxxs-11 11/14 400
```

The **content ladder** is user-adjustable: `--dsh-content-font-size` is an
integer **12–17 px, default 14**; `delta = size − 14`; a secondary tier
`min(size−1, max(13, size−2))` (13 at default) is used for row titles.
Markdown: h1 `21+δ / 30+δ 700`, h2 `19+δ / 28+δ 700`, h3 `18+δ / 26+δ 700`,
h4 `14 / 24+δ 600`, body `14 / 24+δ`, table `13 / 22`, **small text and code
are fixed** (small `12/20`, inline code `12/19` mono, code block `11/19` mono,
IN/OUT card `11/16` mono) and do not follow the preference. Almost every row
in the transcript is `calc(24px + δ)` tall.

**Geometry.** No spacing token set; the rhythm is `2 4 6 8 10 12 14 16 24 32`.
Radii by role: `6` inline code / tool row / stopped tag · `8` inputs, list
rows, tooltip, notice · `10` menu items · `12` cards, code/diff blocks,
settings nav cell, New Session, dock cards · `14` toast, small button ·
`18` medium button (capsule) · `20` menus · `22` user bubble, composer card ·
`24` modal · `32` settings panel · `999` pills and circles. Rounded corners
are `superellipse(1.5)` where the engine supports it, plain arcs otherwise;
egui has only arcs, which is the supported fallback — nothing is lost.

**Hairlines and elevation.** Neutral borders draw at **0.5 px** (one device
pixel); dashed and state-coloured borders stay 1 px. Elevated surfaces have
`border: 0` and take a shadow whose *first layer is the hairline*:

```
elevation-stroke     0 0 0 0.5px <stroke-color>          (default border-l4)
elevation-panel      stroke, 0 3px 8px rgba(0,0,0,.03), 0 0 16px rgba(0,0,0,.02)
elevation-prominent  stroke, 0 3px 8px rgba(0,0,0,.04), 0 0 20px rgba(0,0,0,.05)
elevation-soft       stroke, 0 4px 16px rgba(0,0,0,.03), 0 0 24px rgba(0,0,0,.03)
shadow-lv3           0 0 1px rgba(0,0,0,.2), 0 0 4px rgba(0,0,0,.02), 0 12px 32px rgba(0,0,0,.08)
```

Menus and the model dropdown rebind the stroke to `border-l1`; the composer
card to `border-l2`; the back-to-bottom button to `border-l3`.

**Motion.** `ease-in-out = cubic-bezier(.4,0,.2,1)`; durations `100` (hover
fills, icon crossfade), `120` (chevron rotate), `150` (tooltip in, sidebar
fade, row in), `160` (toast in), `200`, `300` (column slide). Ambient
animations: turn-status text shimmer `1.8s`, tool/reasoning row sweep `2.6s`,
state-dot pixel chase `1s`, toast hold `3000` + fade `1000`. Every animation
has a `prefers-reduced-motion` freeze.

### 1.2 What sica-rust has instead

`Palette` (14 fields) is picked by a `theme_dark` boolean and poured into
`egui::Visuals` in `App::apply_visuals`: rounding 2 everywhere, `Shadow::NONE`,
`inactive` buttons transparent with a hairline, `active` a solid accent fill.
Everything is monospace because Plex Mono is prepended to `Proportional`.
`log_panel.rs` and `status_bar.rs` hard-code colours outside the palette.

### 1.3 Port — `sica_core::theme` v2 + `ui::kit` (**M**)

**Tokens.** Replace `Palette` with two structs and keep `Rgb`:

```rust
pub struct Statics { pub bluish: [Rgb; 19], pub accent: [Rgb; 7], pub green: [Rgb; 3], pub red: [Rgb; 2], pub amber: [Rgb; 4] }
pub struct Aliases {
    pub bg_base: Rgb, pub bg_layer: [Rgb; 3], pub sidebar_fill: Rgb, pub sidebar_hover: Rgb, pub sidebar_active: Rgb,
    pub tip: Rgb, pub bubble: Rgb, pub input_major: Rgb, pub menu: Rgb, pub code_block: Rgb, pub inline_code: Rgb,
    pub label: [Rgb; 4],            // primary, secondary, tertiary, caption
    pub label_dimmed: Rgb, pub label_inverted: Rgb,
    pub border: [Rgba; 4],          // l1..l4 — alpha colours, drawn over whatever is beneath
    pub hover: Rgba, pub active: Rgba, pub hover_danger: Rgba,
    pub primary_fill: Rgb, pub primary_hover: Rgb, pub info_fill: Rgb, pub info_hover: Rgb,
    pub elevated_fill: Rgb, pub floating_fill: Rgb,
    pub business: Rgb, pub business_tertiary: Rgb,
    pub success: Rgb, pub success_tertiary: Rgb, pub warn: Rgb, pub warn_label: Rgb, pub warn_tertiary: Rgb, pub error: Rgb,
    pub tooltip_bg: Rgb, pub toast_bg: Rgb, pub scrollbar: Rgb, pub scrollbar_hover: Rgb,
}
pub struct Theme { pub statics: Statics, pub alias: Aliases, pub dark: bool, pub content_px: u8 /* 12..=17 */ }
impl Theme { pub fn light() -> Self; pub fn dark() -> Self; pub fn system() -> Self /* eframe dark_light_mode */ }
```

Fill the two alias maps from the table above. `tokens` module: `RADIUS_ROW = 6`,
`RADIUS_INPUT = 8`, `RADIUS_ITEM = 10`, `RADIUS_CARD = 12`, `RADIUS_BTN_SM = 14`,
`RADIUS_BTN = 18`, `RADIUS_MENU = 20`, `RADIUS_BUBBLE = 22`, `RADIUS_MODAL = 24`,
`RADIUS_SETTINGS = 32`, `HAIRLINE = 0.5`, `ROW_H = 24.0`, and the three
`Elevation` presets as `Vec<egui::Shadow>` (egui 0.28 `Shadow { offset, blur,
spread, color }` — paint the stroke layer as a `spread: 0.5` shadow with
`blur: 0`, then the two blur layers; `Frame` takes one shadow, so `kit::elevated_frame`
paints the extra layers with `painter.rect_filled` before the frame).

**Fonts.** Bundle **Inter** (Regular / Medium / SemiBold / Bold) as the UI
family and **JetBrains Mono** (Regular / Bold) as the code family in
`crates/frontend/assets/fonts/`; keep egui's defaults in the chain for emoji
and CJK. `FontFamily::Proportional = [inter, …]`, `Monospace = [jetbrains, …]`,
plus named families `inter_medium`, `inter_semibold`, `inter_bold` (egui
has no weight axis, so each weight is a family). Newsreader goes; the tracked
caps label style goes with it — dsh has no caps labels anywhere.

**`apply_visuals`.** `Visuals::light()/dark()` then: `panel_fill = bg_base`,
`window_fill = bg_layer[1]`, `extreme_bg_color = code_block`, `faint_bg_color = tip`;
`widgets.inactive` = transparent fill, `hairline border-l3`, `label[0]`;
`hovered` = `hover` wash, no stroke change; `active` = `active` wash;
`selection.bg_fill = business @ .18`, `selection.stroke = business`;
`hyperlink_color = business`; `window_rounding = RADIUS_MODAL`; `menu_rounding = RADIUS_MENU`;
`popup_shadow = elevation-prominent`. Text styles: `Body 14/inter`, `Small 12`,
`Monospace 12/jetbrains`, `Heading 16/inter_medium`, plus named styles
`"row-title"` (13, secondary tier), `"content"` (`content_px`), `"code-block"` (11 mono),
`"h1".."h3"` from the markdown ladder. `content_px` feeds the `Body`/`content`
sizes; it is a Settings › General stepper (§7).

**`ui::kit` — the primitives** (replace `widgets.rs`). One function per dsh
primitive, named the same so the doc and the code line up:

| `kit::` | dsh primitive | Rules |
| --- | --- | --- |
| `button(ui, label, Variant, Size)` | `Button` | capsule r=18 (md, h=36, pad 0 14) / r=14 (sm, h=28, 12/18, pad 0 10); `Primary` = `primary_fill` + `label_inverted`, hover `primary_hover`; `Ghost` = transparent, hover `hover`; `Outline` = 0.5 `border-l3`; disabled = 40 % opacity |
| `icon_button(ui, Icon, px)` | 28 px circle | transparent, hover `hover`, glyph `label[1]` |
| `pill(ui, text, active)` | `Pill` | h=24, r=12, pad 0 8, 12/18, `label[1]` on `bg_layer[1]`; active = inset 1 px `#979DA6` ring + `label[0]` |
| `state_dot(ui, DotState)` | `StateDot` | 10 px: halo @ 10 % + core inset 20 %; `Done` success, `Warning` warn, `Error` error, `Ongoing` = 2×2 pixel chase in `accent[450]`, 1 s, per-cell −125 ms |
| `disclosure_row(ui, Icon, title, summary, open) -> Response` | `DisclosureRow` | h=`ROW_H+δ`; 16 px leading box (14 px glyph, crossfades to a chevron on hover), gap 6, title 13/`label[1]`, 2×2 dot separator `label[3]` with 8 px margins, summary `label[2]` ellipsized |
| `menu(ui, anchor, side, items)` | `Menu` | `Area` at `Order::Foreground`; card r=20, pad 4, `menu` fill, elevation-prominent with `border-l1` stroke; item min-h 40, r=10, pad 8 10, 14/22; **selected shows a trailing check, never a fill**; `Danger` items in `error` with `hover_danger`; separator 0.5 px `border-l1` margin 4 2; closes on outside click / Esc |
| `modal(ctx, title, body, footer)` | `Modal` | full-window mask `rgba(0,0,0,.24)`; dialog `min(380, avail)` wide, r=24, `bg_layer[1]`, elevation-prominent; header pad 22 14 12 24, title 16/24 500, close 28 px r=8; body/footer pad 0 24; Esc closes |
| `tooltip(text)` | `Tooltip` | `tooltip_bg`, white 13/20, r=8, pad 3 7, max-width 50 % of window, 500 ms delay in the sidebar, fade in 150 ms |
| `toast(ctx, seq, icon, text, hold_ms)` | `Toast` | top-centre of the *conversation column*, 120 px from top; r=14, `toast_bg`, `label_inverted` 14/22, `shadow-lv3`; slide-in 160 ms, hold (default 3000), fade 1000; **no severity kinds** — the caller picks the icon; re-showing bumps `seq` |
| `connection_indicator(ui, state)` | `ConnectionIndicator` | h=32, r=8, 12/18 500; `Disconnected`/`Connecting` = `warn_tertiary` fill + `warn_label` text, hover text "Reconnect now"; `Recovered` = `success_tertiary` + `success`, shown 2000 ms; **hidden when healthy**; fixed width sized to the longest label |
| `code_block(ui, lang, text)` | `CodeBlock` | r=12, `code_block` fill; sticky banner pad 9 14 with the infostring in 11/18 mono left and a Copy → "Copied" (1000 ms) button right; body 11/19 mono, pad 16, wraps |
| `diff_block(ui, DiffView)` | `DiffBlock` | same card, body pad 12 14, `white-space: pre` (h-scroll); header path 600; lines prefixed `+ `/`- ` coloured `success`/`error` as *text only*; hunk gaps and footer `└ +A -R · N file(s)` in `label[2]`; collapsed above 8 lines with `… {n} more lines` |
| `terminal_block(ui, TerminalView)` | `TerminalBlock` | prompt row = state dot + cwd basename (or `~`/`$`) + command, one per command line; `exit code {n}` / `signal {s}` pill; ANSI SGR parsed (`llm`/`agents` already strip; add a small parser in `frontend::ansi`); "No output" empty state; height cap 224 px |
| `io_card(ui, input, output, failed)` | tool IN/OUT card | r=12, 0.5 `border-l1`, `code_block` fill, 11/16 mono; two sections `IN` / `OUT` with sticky gutter labels in `label[3]`, each `max-height: 150` and independently scrollable, 0.5 `border-l2` divider; failed OUT in `error` |
| `hairline(ui, Level)` | separators | 0.5 px `border-l1..l4` |
| `elevated_frame(Elevation, stroke_level)` | menu/popover/card chrome | paints the shadow stack described above |

Icons: dsh ships 74 `currentColor` SVGs at 8/12/14/16/20 px. egui cannot
paint SVG on its own. The ~30 glyphs the transcript needs are hand-painted
with `Painter` behind the `Icon` enum, so the choice stays local to
`ui::icons`. The *brand* marks are the exception: `resvg` is a direct
workspace dependency and `ui::icons::mark` rasterises `assets/mark.svg`
once into a white texture that is tinted per theme, which is also how
`icon::generate` builds the window icon from `assets/icon.svg`. Glyphs stay
hand-painted because a 16 px stroke drawn by `Painter` beats a downscaled
raster; the mark does not, because it has curves, a mask and a gradient.

---

## 2. Shell layout

### 2.1 What dsh does

`ui-layout/src/client/AppFrame.tsx` + `columns.ts`: a **three-column grid**,
one row, `[sidebar] [center minmax(0,1fr)] [details]`, on `bg-base`, with a
300 ms column-track transition (paused while dragging).

```
SIDEBAR_DEFAULT 280   SIDEBAR_MIN 264   SIDEBAR_MAX 420   SIDEBAR_COLLAPSED 56
SIDEBAR_AUTO_COLLAPSE 1024 (viewport)   CENTER_MIN 640
DETAILS_DEFAULT 360   DETAILS_MIN 300   DETAILS_MAX 520   (0 = closed, still mounted)
```

Concession order when the window is too narrow: shrink details → close
details → the sidebar never concedes. Drag handles are invisible 8 px strips
centred on the seam; only the details handle shows a 12×32 pill on hover.
Columns are separated by 0.5 px `border-l3`. **Sidebar collapsed state and
widths are not persisted** — every launch starts at 280 expanded, details
closed; below 1024 px the sidebar auto-collapses via a separate override so
re-widening restores the layout exactly.

**Sidebar** (`ui-sidebar/SidebarRoot.tsx`), `sidebar-fill`, pad `6 12`, top to bottom:

1. **Brand row** h=60 (collapsed 36): 24 px mark + wordmark (18/24 600,
   letter-spacing .04em) — the brand *is* a New Session button; a 28 px
   circle panel-toggle on the right. Collapsed: the toggle shows the mark at
   rest and swaps to the panel icon on hover.
2. **New Session** h=38, r=12, 0.5 `border-l3`, `elevated_fill`, 14/22 500,
   icon 14; collapsed → 36×36 icon with a 500 ms tooltip.
3. **Session region** (`flex: 1`, own scrollbar at the column edge, 24 px
   bottom fade to `sidebar-fill`) — §8.
4. **Foot**: the Settings trigger (h=42, r=12, 14/22, gear icon; collapsed
   36×36 circle) with the **connection indicator inline to its right**
   (wide mode only).

Collapse choreography: content freezes at its expanded width, fades 150 ms,
rail icons slide in from +49 px over 150 ms while the track finishes its
300 ms slide. Scrollbar thumbs are transparent unless the pointer is inside
the column, lingering 2000 ms after it leaves.

**Conversation column** (`ui-conversation/skeleton/ConversationRoot.*`):

```
--dsh-chat-content-width = clamp(680px, column-width × 0.64, 920px)   (user-draggable)
composer card max-width  = content + 32px
```

Header pad `12 28 0 20`, a 0.5 px `border-l3` seam; title row min-h 32 with
breadcrumb crumbs (max-w 220, pad 4 8, r=12, 14/20 `label[2]`, current
500 `label[0]`, `/` separators in `label[3]`); header actions right (jobs
popover, schedule popover, subagent lineage). Under it a **tab strip**
(`Chat` · `Trajectory`) only when >1 view is registered: gap 36, 13/16 500,
`label[2]`, active = `business` text + 2 px r=2 underline.

The composer seat is `position: sticky; bottom: 0` with a 36 px gradient
fade from transparent to `bg-base` over the transcript. In the **hero phase**
(blank session) the whole stack is vertically centred with `padding-bottom:
32px`, and the header is hidden.

**Details column** hosts one slot, `conversation.details.tool` — the
selected tool call's full payload (empty state: "Click a tool row in the
message flow to view its details").

### 2.2 Port (**M**, layout only; details column **S** on top)

Panel order in `ui::draw`:

```rust
SidePanel::left("sidebar").exact_width(app.layout.sidebar_w)   // 280 | 56; resizable(true) when expanded, clamp 264..=420
SidePanel::right("details").exact_width(app.layout.details_w)  // 0 = skip the panel entirely
CentralPanel                                                    // conversation | (settings is a modal, §7)
```

`LayoutState { sidebar_w, details_w, narrow, narrow_expanded }` on `App`,
transient (not in `sica-settings.json`), with the auto-collapse rule at
`ctx.screen_rect().width() < 1024`. The bottom status bar goes: its facts
move to the composer ring (§6), the connection indicator (§9) and the
session header. Column seams: `show_separator_line(false)` and paint a
0.5 px `border-l3` vline. Drag handle: an invisible 8 px `Sense::drag()` rect
on the seam; egui's own resize handle is turned off (`resizable(false)`) so
the width is ours to clamp and animate (`ctx.animate_value_with_time`,
300 ms). The 150 ms fade/slide choreography is a nicety; ship the width
animation first.

The rail's two buttons (Chat / Settings) are replaced by the dsh foot:
Settings becomes a **modal** (§7), so the sidebar has no view switch at
all — collapsed it shows brand, New Session, and the gear.

`chat::draw` gets a **header**: session title crumb (clickable → rename),
right-aligned actions (`jobs` popover, goal indicator if not in the dock,
subagent count), then the tab strip when the Trajectory view (§10) exists.
Content width: `let w = (avail * 0.64).clamp(680.0, 920.0).min(avail - 64.0)`;
centre a `w`-wide column for the transcript and `w + 32` for the composer.
Persist a user override in `sica-settings.json` as `chat_content_width`.

Hero phase: when the active session has no `UserMessage`, hide the header,
vertically centre `[headline] [composer]` with 32 px bottom bias; the
headline is the blade mark at 38 px + "sica" in 26/32 500 + the "Preview"
superscript style badge reading the build profile (`debug`/`release`).

---

## 3. Transcript

Everything here is `ui-chat`, `ui-tool`, `ui-primitives`. The flow is one
column of **flow items** separated by 16 px (8 px between a collapsed
process fold and its answer). No avatars, no role labels anywhere.

### 3.1 User message

**dsh:** right-aligned bubble, `specific-bubble` fill, **r=22**, pad `10 16`,
14 px / `22+δ` line-height, max-width `min(content × 0.702, 82 %)`;
pre-wrap. `@file`, `@"quoted"`, `/command` tokens inside the text render as
inline chips in `business` 500 with a 1 em kind icon. Under the bubble a
hover-revealed action row (28 px circles: copy · timestamp before the icons
in `label[2]` 13 px). Images: one image renders at a 240 px long edge,
several as 64×64 tiles, r=16, 0.5 `border-l2`, zoom-in cursor → lightbox.

**sica-rust:** `messages::draw_user` — right-aligned `surface_sunk` box r=2,
pad 14 10, 78 % max width; "queued — runs when the current turn ends"
caption below when `turn.queued`.

**Port (S):** swap chrome to `bubble`/r=22/pad 10 16/70 % max; the
`queued` caption goes — queued messages live in the composer dock (§5) until
the BE admits them; `InboxChanged { accepted: "running" }` is when the bubble
appears in the flow. Decorate `/name` and `@path` runs via a small tokenizer
in `frontend::user_text` painted as `RichText` spans (the BE already records
`ContextInjected { source: SkillInvocation }` for a leading `/name`).

### 3.2 Assistant message

**dsh:** flat, full column width, `label[0]`, 14 px / `24+δ`, 16 px between
blocks; markdown per §1.1 (code block card with sticky language banner +
copy; tables 13/22 with 0.5 px row rules; blockquote 2 px `label[3]` left
rule; inline code r=6 pad 0 5). **No streaming caret.** While a turn runs the
tail shows a `TurnStatus` line: **"Deep diving..."** as a shimmering gradient
text (`accent[500] → accent[200] → accent[500]`, 250 % background, 1.8 s
linear), h=`26+δ`, 14/500; after 15 s a `label[3]` 13 px mono clock
(`{s}s` / `{m}m {s}s`) appears beside it. An interrupted reply ends with a
**"Stopped"** tag: pad 0 6, r=6, `hover` fill, `label[2]`, fixed 11/18.

**sica-rust:** "ASSISTANT" caps + hairline + `CommonMarkViewer` + Copy;
the working strip (`working_sweep` + "Thinking"/"Running · name") sits
*below* the tool chips.

**Port (S–M):** drop the label and rule. Keep `egui_commonmark` for the body
but restyle through `CommonMarkCache`/style overrides: code blocks through
`kit::code_block` (egui_commonmark 0.17 lets you intercept fenced blocks via
`CommonMarkViewer::…` render hooks — if not, pre-split the markdown on fences
and render code blocks ourselves between viewer calls, which also gives the
copy button and language banner). Replace `draw_working` with
`TurnStatus`: shimmer = a `LayoutJob` whose per-glyph colour is sampled from
a moving gradient each frame (`ctx.request_repaint_after(16ms)` while
streaming); the "Deep diving..." string is a `strings::TURN_STATUS` constant
(sica wording: "Working…"). The stopped tag keys off
`finish_reason == "interrupted"`.

### 3.3 Reasoning

**dsh** (`ReasoningRow`): a `DisclosureRow` — think icon, title **"Think"**,
dot, one-line summary: while running the *latest* reasoning line
(right-anchored so text scrolls in), when settled the *first* line;
`label[2]` 13/20 ellipsized. Running rows carry a **sweep glare**: a 300 px
band of `bg-base @ 60 %` gliding left→right, 2.6 s ease-out. Expanded body is
plain text (not markdown), 13/20 `label[2]`, indented `22+δ`. No duration.

**sica-rust:** `+ REASONING` / `− REASONING` / `· REASONING (LIVE)` caps
chips and an italic-serif body with a blue rule.

**Port (S):** `kit::disclosure_row(Icon::Think, "Think", summary, open)` +
plain-text body in the secondary tier. Sweep = a translucent gradient rect
painted over the row while `!finished`, `x = (t / 2.6).fract() * (w + 300) - 300`.
Keep `reasoning_collapsed` semantics (auto-collapse on `TurnFinished`).

### 3.4 Tool calls

**dsh** (`ui-tool/ToolRow.*`, `tool-call-model.ts`): a **row, not a card**,
`[16 px icon] 6 [title 13/24] 8 [2×2 dot] 8 [summary flex, ellipsized] [suffix]`.

Variants → icon → title: `search` (magnifier, "Search"), `read` (browse,
"Read"), `bash` (api glyph, "Bash"/"Pwsh"), `write`/`edit` (pencil, "Write"/
"Edit"), `code` (code, "Code"), `others` (sparkle, "Tool call", summary
becomes `{tool} · {base}`). Summary = first non-empty of a per-variant key
list (`bash: description, command` · `read: path, url` · `search: query,
pattern` · `write/edit: path`), paths relativised to cwd and `~`-shortened;
for read/write/edit the summary is a dotted-underline **file link** that
opens the file in the OS.

States: `running` → tool icon + the same 2.6 s sweep, hidden text "Running";
`ok` → plain; `error` → leading replaced by a red `StateDot`, summary
replaced by the failure's first line in `error`; `stopped` → amber dot. **No
per-call duration** on the row. Collapsed edit rows carry `+A -R` in mono
`label[3]` (`.diffStat`).

Expanded body (click the row; the body is a sibling so clicks in it never
toggle), first match wins: ask-question card → **terminal block** (bash) →
**diff block** (edit/write) → **read block** (line-numbered, 8-line cap
`Showing {shown} of {total} lines`) → image gallery → **search block**
(`{n} matches · {f} files`, 8-line cap) → web block → the generic **IN/OUT
card**. A hover-revealed **"Inspect"** pill (0.5 `border-l3`, r=999, 11/16)
under the body opens the Trajectory view focused on that call.

Nested calls (`ToolCallTree`): children in a column with `gap 4; margin 4 0 2
22px; padding-left 8px; border-left 0.5px border-l2` — 22 px indent + guide
line, recursive. **Parallel calls have no grouping chrome** — they are
consecutive sibling rows.

**sica-rust:** `tool_chips::draw` — one `horizontal_wrapped` row of
`RUN/OK/ERR` + caps name chips with hover tooltips; depth and parent are
carried on `ToolChip` and never drawn.

**Port (M):** replace `tool_chips.rs` with `ui/chat/tool_row.rs`:

```rust
pub enum ToolVariant { Search, Read, Bash, Write, Edit, Code, Other }
impl ToolVariant { fn of(skill: &str) -> Self }   // run-cli/run-pwsh→Bash, read-file→Read, glob/grep→Search, write-file→Write, edit-file→Edit, subagent*/ralph/agent-team→Other …
pub enum ToolState { Running, Ok, Error, Stopped }
```

Each `ToolChip` becomes one `disclosure_row`; children are found by
`parent_id` and drawn recursively inside a 22 px-indented column with a
0.5 px vline. The body needs the **full args and output**, which
`ToolCallStarted/Finished` and `MessageDump` do not carry today
(`args_preview` is truncated, `summary` is the expectation-summarised
text). Protocol (§11): add `args_json: String` to `ToolCallStarted` and
`output: String` (spill-aware: the head/tail the model saw) to
`ToolCallFinished`, and `tool_args_json` / `tool_call_id` / `tool_parent_id`
/ `tool_depth` to `MessageDump` so history chips regain their tree. The
edit-file skill's result already contains the replaced span; render a diff
by computing `similar`-style line diff on the FE from `old`/`new` args
(`edit-file` args are literal). Terminal block: parse the `run-cli` result's
`[stdout]`/`[stderr]`/`exit` framing (`builtins::run_shell` output shape).

### 3.5 Turn structure

**Turn process fold** (`TurnProcessNodeView`, only in the **Compact**
transcript setting, for closed turns): a full-width 33 px button with a
0.5 px `border-l2` bottom rule, label `{n} tool calls · {n} messages · {n}
subagents` (or **"Thought for a while"** when all zero), chevron rotating
−90° → 0°. Everything between the user message and the final answer folds
behind it. **Normal** mode shows every row.

**Turn tail** (`TurnTailNodeView`): produced-files chips (`Produced` label +
up to 6 r=6 chips, `+ n files` overflow), then the icon-action row: copy ·
[thumbs] · branch ("Branch into a new conversation", disabled unless last
message of a completed turn) · **Usage pill** (`Usage 12.2K`, dialog:
provider/model, cache hit %, uncached/cached input, cache write, output
(+reasoning)) · **Time pill** (`Ran for 42s`, dialog: total, tok/s, TTFT)
· timestamp. Reveal: always on the newest turn, on hover for older ones.

**Retry chain** (`ModelRetryItem`, a `<details>` row): `{label} ({retry}/
{max}) · {seconds}s` with label ∈ "Retrying model request" / "Waiting to
retry model request" / "Retried model request" / "Model request retry
cancelled"; live 250 ms countdown, shimmer while active; body `Retry delay:
{ms}ms`, `Failure reason: …`.

**Turn error** (`TurnErrorItem`): grid `10px 1fr auto`, red dot, **"This
turn failed"** in `error` 600, message in `label[1]`, error code as 11 px
mono `label[2]`. **Max tokens**: amber dot, **"Output token limit reached"**,
hint "The reply was cut off; earlier output is preserved in the
conversation. Send "continue" to let the model resume."

**Compaction marker** (`CompactionItem`): a quiet one-line button, h=`24+δ`,
r=6; icon crossfades to a chevron on hover; title **"Context compacted"** in
`label_dimmed` 13 px; dot; summary `Compacted {items} history items
(~{tokens} tokens)`; expanded body renders the summary as markdown, indented
`22+δ`. It does *not* hide the shadowed rows. Manual `/compact` titles
`compact`.

**Context injection** (`ContextInjectionRow`): a disclosure row, icon
`context-injection`, title **"Context injection"**, dot, producer label,
optional summary; body is a scrollable `max-height: 141` r=8 `code_block`
box in 11/16 mono with **form-specific renderers**: `instructions` (file
list with loaded/added/updated/removed + raw text, `<system-reminder>`
framing kept verbatim because "the framing is part of what the model read"),
`catalog`, `snapshot` (`Supersedes earlier snapshots`), `notice`, `relay`
(`From session {id}`). The whole system prompt gets its own **"System
prompt"** row at the top.

**sica-rust:** notices are centred caps lines between half-strength rules
(`draw_notice`) — compaction, steer echoes, loaded-skill markers; retries are
invisible (only `LogLine`); errors are `Request failed · see log`.

**Port (M):**

- `Turn` gains `events: Vec<TurnRow>` where `TurnRow ∈ { Reasoning, Tool(id),
  Injection(ContextSource, body), Compaction(summary, folded, tokens),
  Retry(attempt, max, delay_ms, reason), Error(code, msg), MaxTokens,
  Stopped }` in arrival order — the current `notice` field folds into it.
- Wire: `SessionDump.messages` already carries `role: "context"` + `context_source`
  — **start reading it** (`rebuild_turns`). Add `Event::LlmRetry { session_id,
  attempt, max, delay_ms, reason }` (the BE appends the durable `LlmRetry`
  event today but pushes only a `LogLine`). Add `finish_reason: "max_tokens"`
  passthrough (llm returns `length`; map it in `chat.rs`).
- Compact/Normal is a Settings › General row (`transcript_compact: bool`);
  in Compact, closed turns render `[user] [fold button] [answer] [tail]`.
- Per-turn usage: `TokenUsage` is per-session cumulative today; add
  `Event::TurnUsage { session_id, turn_id, prompt, completion, reasoning,
  duration_ms, ttft_ms }` emitted from `chat.rs` at `TurnEnd` (the `llm`
  crate has the `usage` trailer and timestamps). The tail pills read it.

### 3.6 Scroll behaviour

**dsh** (`ChatView.tsx`): "at bottom" = within **24 px**; a scroll is
attributed to the reader only if it moved more than 0.5 px from the last
programmatic position, which is what stops streaming from stealing the
scroll mid-inertia. Auto-follow fires only when the *tip signature* changes
(`openState:firstSeq:lastKey:count:running:…`), never on a re-render. A newly
appended user/steer message force-scrolls. **Back to bottom**: a 34 px circle
in a zero-height sticky slot 16 px above the composer, `floating_fill`,
elevation-panel, chevron-down, only while `!atBottom`. **Load earlier**: a
centred r=14 12 px button at the top of the column, pages 50 nodes, keeps the
reader's row in place. A right-gutter **turn rail** (10 px pitch ticks, hover
preview card, hidden under 900 px) scrubs between turns.

**sica-rust:** `stick_to_bottom(!autoscroll_paused)`, manual
`state.offset.y = max` snap, a "↓ FOLLOW" pill at the bottom centre,
middle-click pan, Ctrl+A/Ctrl+C message selection.

**Port (S):** keep the mechanism; restyle the pill as `kit::icon_button`
34 px circle with `floating_fill` + elevation-panel, anchored
`composer_top - 16 - 34`; 24 px threshold; drop the caps label. Keep
Ctrl+A/C and middle-click pan (dsh has neither; they are sica-rust
strengths). Pagination and the turn rail are **L / later** — sessions load
eagerly today.

### 3.7 Empty state

**dsh:** headline `[34 px mark] [26/32 500 title] [Preview badge]`,
workspace chip beneath (r=16, folder icon + name + chevron), composer with the
hero placeholder "Describe what you want to build... / commands, @ files or
sessions". The composer is *not* remounted between hero and active phases.

**Port (S):** `draw_empty` → the hero described in §2.2; the placeholder
swaps per phase (§5).

### 3.8 Markdown extras — math, images, wide tables, file links — **done** (UI-8)

**dsh** (`ui-renderer` and the markdown pipeline behind §3.2): **math** —
inline `$…$` and `\(…\)`, display `$$…$$` and `\[…\]` typeset with KaTeX,
numbered display equations (`(1)`), math inside table cells; **images** — a
markdown image renders inline in the body (durable attachments through the
§5.3 gallery, remote URLs as they are); **wide tables** scroll horizontally
inside their own container, never the column; **inline-code file paths**
become links — a runtime-context line tells the model that when it creates or
modifies files it should name the primary outputs in its final answer as
inline code carrying the exact tool path (or a basename unique among that
turn's changed files), and the renderer resolves those to clickable
references; **CJK** strong text is rendered where CommonMark's word-boundary
rule would drop it; every link is clickable and opens externally.

**sica-rust:** `egui_commonmark` — no math, images only if a loader is
installed, tables at natural width (a wide one pushes the column), inline
code is plain.

**Port:**
- **Math (S, honest fallback):** there is no KaTeX for egui. Detect `$…$`,
  `$$…$$`, `\(…\)` and `\[…\]` before the viewer and render the TeX source
  through `kit::code_block` tagged `math` (display) or as inline code
  (inline), so the source stays legible and copyable instead of being
  mangled by the parser's `_` and `*` rules. Typesetting is **later**.
- **Images (S):** add `egui_extras` with its image loaders and install them
  once at startup; allow relative and `file://` paths only under the
  session's `cwd` (§4.3) plus `http(s)://`; a failed load shows the alt text
  with a retry glyph, as dsh does.
- **Wide tables (S):** wrap each table in a horizontal `ScrollArea` capped at
  the content width — a pre-split on the table block, like the code-fence
  split in §3.2.
- **File links (S, FE + one prompt line):** add dsh's runtime-context line to
  `agents::prompt` (harness §5.1), and in the viewer treat inline code that
  resolves to an existing file under `cwd` as a link: hover shows the full
  path, click opens it in the OS (`open_in_explorer`), Ctrl-click inserts
  `@path` into the composer. The produced-file chips (UI-6) already do this
  for `write-file` rows; this extends it to prose.

**Math and wide tables, as shipped.** `ui::chat::md_blocks::split` cuts an
assistant message into `Prose | Math | Table` **before** the viewer sees it,
and `draw_assistant` renders each with what suits it: prose through
`CommonMarkViewer` (with `inline_math_to_code` first), display math through
`kit::code_block` tagged `math`, a table through the viewer inside its own
horizontal `ScrollArea`.

Three rules the tests pin, because each is a way to lose the reader's text.
**A fence is opaque** — a `$$` or a `|` inside one is code the model is
*showing*, and splitting there would tear the fence in half. **Money is not
math** — `$5 and $6` keeps its dollars; a formula needs a non-space after
the opening delimiter and before the closing one, which is where dsh draws
the line too. And **every prefix of a message renders**: the stream arrives
a token at a time, so an unclosed `$$` or fence stays prose rather than
vanishing into a block that is waiting to be finished.

Math is a deliberate non-port, not a stub: there is no KaTeX for egui, and
a half-typeset formula is worse than none, because CommonMark's `_` and `*`
rules turn `x_1` into italics and the reader cannot recover what was
written. The source, legible and copyable, is the honest fallback.

**Images and file links, as shipped.** `egui_commonmark`'s `load-images`
feature turns the loaders on; the work is the fence. Left alone, a bare
`![x](a.png)` becomes `file://a.png` resolved against **the process's**
directory — wherever the app was launched from — which is never what the
model meant, since it writes paths relative to the folder it works in. So
`rewrite_image_uris` resolves local targets under the session's `cwd`
(§4.3), and **refuses anything that climbs out**: prose is not a
capability, and an assistant writing `![](../../secrets.png)` must not
thereby get the app to open it. A refused target is left visible as inline
code rather than silently not appearing.

File links are the same fence pointed the other way. `linkify_file_paths`
turns inline code into a link **only** when it names a file that is really
there and really inside `cwd`, and it declines anything that does not look
like a path at all (a space, no dot, a leading dash) — guessing wrong turns
ordinary prose into a wall of links. The click is intercepted in
`App::take_file_link` *before* the platform sees it, because the platform
would hand a local file to the browser: a plain click opens it the way the
OS would, and a modified click (egui reports it as "new tab") puts
`@path` in the composer instead — reading it versus bringing it into the
next message.

The prompt half is `prompt::OUTPUT_NAMING`, one runtime-context line asking
the model to name what it produced as inline code with the exact path. It
costs **~40 tokens per request**, which is what re-blessed all five replay
recordings; that diff — the line and the token count, and nothing else —
is what a prompt change should look like.

---

## 4. Sessions and the sidebar

### 4.1 What dsh does

`ui-workspace/rows/*`. Sessions are grouped **by workspace** (or one flat
recency list), ordered **Manual** (drag) or **Last updated**. A collapsed
group shows 5 sessions + "Show {n} more". Row h=32, r=8, pad 0 8:
`[16 px status slot] 4 [title 14/20] 6 [alarm marker?] [relative time 12/20 label[2]] [⋯]`;
hover swaps the time for a ⋯ menu (**Rename · Fork session · Archive
session**). No subtitle line except in search results.

Status is **one `StateDot`** by precedence: pending approval / plan review /
question → amber ("Waiting for approval" / "Plan awaiting review" / "Waiting
for answer"); running (or any subagent descendant running) → blue chase
("Running"); completed-since-last-viewed → green ("Completed", cleared on
open); idle → **no dot**. Timestamps are relative buckets `now · {n}min ·
{n}h · {n}d · {n}mo · {n}y`; absolute time only in the hover card.

Search: a header icon that expands into a 30 px r=10 field; immediate
title matches plus a **250 ms debounced** host content search with
snippets, capped at 20. Rename pins the automatic title. Archive has no
confirmation and no unarchive UI. **There is no delete session** and no
keyboard switching.

### 4.2 Port (**M**)

UI-4 shipped the **flat list, Last updated**, sorted by the newest event;
grouping by workspace is §4.3, on the harness registry (guide §3.9). Row chrome per above.
`SessionMeta` needs `updated_at`, `running`, `pending: Option<PendingKind>`
and `completed_unseen: bool` — the last three the FE derives itself from
`TurnStarted/Finished`, `ApprovalRequested/QuestionAsked`, and "finished
while not the active session". Add `updated_at` to `SessionMeta`
(protocol) — the BE has the last event's timestamp.

Row menu: **Rename** (`Request::RenameSession { id, title }` — new; the BE
appends `SessionTitle` and marks it user-pinned so `title_gen` skips it),
**Fork** (`Request::ForkSession { id }` — new; reuse `chat::fork_seed` to
copy completed turns into a fresh log), **Archive** (`Request::ArchiveSession`
— new; a durable `Archived` event hides the row) and keep sica-rust's
**Delete** behind the same armed Delete/Keep confirmation, because the file
is the user's and dsh's "no delete" is a server-product decision. Search:
FE-side title filter immediately; content search is `Request::SearchSessions
{ query }` over the JSONL (**S** on the BE, `sessions_store` already holds
the logs).

The sidebar header row ("Sessions", search icon, `+`) uses h=36, `label[2]`;
the 24 px bottom fade is a gradient rect painted over the scroll area.

### 4.3 Workspaces — grouping, the picker, and "Add workspace" — **done** (UI-8, on harness §3.9)

**dsh** (`ui-workspace`, `ui-directory-picker-{native,browse}`):

- **Section header** h=36, r=12: the label **"Workspaces"** (grouped) or
  **"Sessions"** (flat) in `label[2]`, then a search icon that expands to
  fill the header (§4.1), a **View options** menu (**Group by:** WorkSpace ·
  In one list; **Order by:** Manual · Last updated) and **Add workspace**.
  Entering Last updated re-sorts fully and later prompts promote their
  session once; entering Manual freezes positions and drags edit the order.
  Workspace order is host-durable in either mode; session order inside a real
  workspace is host-durable only in Manual; Ungrouped and flat orders stay
  browser-local.
- **Workspace row** h=34, r=8, pad 0 8, gap 6: `[folder 16] [title 14/20] …
  [⋯ "Workspace actions for {name}"] [+ "New session in {name}"]`, the two
  trailing actions 28 px circles; click toggles the group. Expanded: the
  first **5 non-blank sessions** plus the selected blank **"New Session"** as
  one provisional row until its first prompt, then **"Show {n} more
  sessions" / "Show less"**; closing and reopening restores the fold.
  Sessions without a workspace sit under **"Ungrouped"**; `origin: subagent`
  sessions are hidden everywhere; a row inherits the blue activity dot while
  any subagent descendant runs ("{n} subagents running").
- **Workspace menu:** **Rename workspace** (dialog, field "Workspace name"; a
  duplicate answers "A workspace named “{name}” already exists."), **Delete
  workspace** (dialog: "This removes “{name}” from the workspace list. The
  folder and session logs will be kept. Its sessions will appear under
  Ungrouped." · Cancel / Delete · "Deleting workspace…"), **Add workspace…**.
  The session menu adds **Fork session** / **Archive session** (§4.1). Empty
  states: "No sessions yet", "No matches".
- **Hover card** on a workspace row: the path with the home directory
  shortened to `~` (`~/Documents/project`) and "Created {time}"; click copies
  the *full* path and flashes "Copied".
- **Hero picker** (§3.7's chip, r=16 `[folder] [name] [chevron]`): a menu of
  workspaces ("Loading workspaces…" while the baseline loads) with **Add
  workspace…** last; the pick is staged for the session about to start and
  adopted only after the list projection has refreshed with the committed
  workspace. A folder that cannot be adopted opens **"Couldn’t open folder"**
  with the host's reason and a **Choose again** button.
- **Add workspace** goes through the composed picker: **native** = the OS
  folder chooser, one per open, cancel decides nothing; **browse** = the
  in-app **"Select Workspace Directory"** dialog, **680 × 500** (clamped to
  the viewport − 32), Miller columns: a header with the crumb trail and an
  editable path zone ("Edit path"), one full-width level until a row is
  selected, then level + children columns; a prefix filter on the last pane;
  footer **"Show hidden files"** toggle (hidden = the host's dot-prefix
  flag), **New folder** (nested dialog "New folder in “{name}”", field
  "Folder name", default "Untitled folder", Create / Cancel; the created
  folder is selected), **Open** adopts the selection, falling back to the
  listed level, / **Cancel**; a level cut at 1000 entries says "Too many
  folders to list; only the beginning is shown."

**sica-rust:** one folder for the whole app (Settings › General "Working
directory": the app folder · five recents · "Choose folder…" through `rfd`;
changing it restarts the BE), a flat session list, and the hero chip that
names the folder and copies its path.

**Port (M):**

- **State.** `App.workspaces: Vec<WorkspaceDump>` + `ungrouped: Vec<u64>`
  from `Response::Workspaces` / `Event::WorkspacesChanged`; per-workspace
  `expanded` and `show_all` flags, and the Group-by / Order-by choice in
  `sica-settings.json` (`sidebar_group: workspace | flat`, `sidebar_order:
  manual | updated`). Legacy sessions with no `cwd` are Ungrouped.
- **Sidebar.** `sidebar::session_region` gains the workspace row (34 px,
  folder icon, ⋯ and + circles), the 5-row fold with "Show {n} more
  sessions", the Ungrouped group, and the View-options `kit::menu`. Row
  status dots and the subagent bubble-up already exist (`row_dot`); a
  workspace whose `missing` flag is set paints its title in `warn` with the
  tooltip "Folder not found — sessions still open" and disables its `+`. The
  workspace ⋯ menu: Rename (inline, as for sessions), Delete (the armed
  Delete/Keep row with dsh's retention sentence as its tooltip — the same
  pattern as session delete), **Move up / Move down** in place of drag (egui
  has no list drag-and-drop; `MoveWorkspace { before }` is what both send).
  The hover card is the session hover card with the `~`-shortened path (a
  `home_dir()` prefix swap) and "Created {time}"; click copies the full
  path.
- **Add workspace** = `rfd::FileDialog::pick_folder` (the Settings row uses
  it already) → `Request::CreateWorkspace { path, title: None }` → on
  `WorkspacesChanged` select the new group and send `NewSession {
  workspace_id }`. A refused path (not a directory, no permission) is the
  `Error` text in a `kit::modal` with **Choose again**, per dsh. The browse
  dialog is **not ported**: FE and BE always share the machine.
- **Hero picker.** The chip becomes a `kit::menu` (title + path detail, the
  current one checked, "Add workspace…" last with `sep_above`). The pick is
  stored as `App.pending_workspace` and spent by the next `NewSession`; the
  header crumb (§8) shows the *session's* workspace, not the app-wide folder.
- **Settings › General.** The "Working directory" row becomes **"Default
  folder for ungrouped sessions"** (which is what `SICA_WORKING_DIR` still
  means after harness §3.9) and stops restarting the BE. The sidebar's `New
  session` button creates in the *selected* workspace, or in Ungrouped when
  none is selected — dsh's provisional "New Session" row under the group.
- Not ported: drag reorder (Move up / down covers it) and browser-local
  session orders (one order, host-durable).

**What shipped.**

`App.workspaces: WorkspacesUi` holds the backend's projection plus the fold
and menu bookkeeping; nothing about membership, order or titles is edited
locally, because the registry is the backend's. `ListWorkspaces` goes out
once per IPC connect and `Event::WorkspacesChanged` carries every later
change — both land on the same `UiEvent`, so there is one reconciliation
path.

The header label reads **Workspaces** or **Sessions**, and the grouping only
appears once a workspace exists: an app that has never registered one still
shows the flat list it always did, rather than a single "Ungrouped" heading
that explains nothing. Beside the search icon sit **View options** (Group by
workspace · One flat list · Add workspace…, the choice persisted as
`sidebar_group`) and **Add workspace**.

A group row is 34 px: chevron, folder, title, and a trailing count that
hover swaps for the `+` and `⋯` circles — the same trade a session row makes
with its timestamp. A workspace whose folder is gone paints its title and
icon in `warn`, says so in the hover card, and its `+` is dimmed and
refuses. Groups fold at five sessions behind "Show {n} more sessions" /
"Show less", **except that the active session is never folded away** — a
fold that hides the selection reads as the session having vanished.
Ungrouped is the last group and has no actions of its own.

The `⋯` menu is New session here · Rename workspace (inline, like a session
rename) · Copy path · Move up / Move down · Add workspace… · Remove
workspace, the last one arming the same two-step Delete/Keep row the session
rows use, with dsh's retention sentence as its tooltip. **Add workspace** is
`rfd::FileDialog::pick_folder` → `CreateWorkspace`; the answer is a
`WorkspacesChanged` carrying the new row, and the FE then opens a session in
it — which is what the user pressed the button for. A refused folder becomes
the **"Couldn't open folder"** modal with the host's own reason and **Choose
again**. The sidebar's own New session button creates in the *active
session's* workspace, or Ungrouped when it has none.

One backend change fell out of building this: `ChatHub` now republishes the
projection on **every** change to the session set — create, delete, archive,
fork — not only when a workspace is explicitly attached. Membership follows
the directory in a session's header, so a session created with no workspace
id still joins the workspace whose path it names, and the first version of
this missed exactly that: a new session appeared in no group until the next
mutation.

**Left for later.**

- **Order by (Manual · Last updated)** is in the View menu, kept as
  `sidebar_order`. It is one choice for the whole sidebar rather than
  dsh's per-workspace one, because the registry has nowhere durable to
  keep a per-workspace answer — and **Ungrouped stays newest-first**
  either way, since it has no manual order to honour: `MoveSession`
  reorders a workspace's account, and Ungrouped has none.
- **The hover card does not copy on click** — an egui tooltip cannot take
  one. The path is in the card and **Copy path** is in the `⋯` menu.
- **The hero picker and the header crumb are in**, with one deliberate
  difference from dsh. dsh stages the pick for a session that does not exist
  yet; here the session is already open, and its folder is stamped into its
  header when it is created and never changes after (harness §3.9). So
  picking a workspace from the chip **starts a session there** rather than
  moving the current one — which costs nothing, because the empty session
  being looked at has never been flushed. The chip and the crumb both name
  `App::session_workspace()`: the session's own workspace, or the app-wide
  folder for an Ungrouped one.
- **Settings › General** still says "Working directory" and still restarts
  the backend. Renaming it to "Default folder for ungrouped sessions" *and*
  dropping the restart needs a request that sets it on a live backend;
  protocol v26 has none, and `SICA_WORKING_DIR` is read from the child's
  environment at spawn.

---

## 5. Composer

### 5.1 What dsh does

`ui-conversation/skeleton/InputBar.*`, `input/editor/keymap.ts`,
`input/submission-policy.ts`, `queue/QueueDock.*`.

**Card:** r=22, `input_major` fill, elevation-soft with `border-l2` stroke,
pad-top 10, 12 px gap between the text and the toolbar, max-width `content +
32`. Text area grows to **336 px (14 lines)** then scrolls inside; hero
variant has a 52 px (2-line) floor. Caret is `business`. Placeholder in
`label[3]`, chosen in this order: disabled → parent offline → steer-queue →
plan → default:

```
default      Message or run a task... / commands, @ files or sessions
hero         Describe what you want to build... / commands, @ files or sessions
workspace    Choose a workspace to start
steerQueue   Cmd/Ctrl+Enter steers all queued messages
plan         describe your task to generate plan
```

**Toolbar row** (pad 2 8 6, gap 12, wraps): left `[+ 28 px circle, opens the
/ menu] [permission chip] [plan chip if on]`; right `[model select]
[context ring] [subagent stop?] [send/stop 34 px circle]`. The send circle is
`info_fill` with a white arrow-up; while running with an empty draft it
becomes a rounded-square stop ("Stop generating"); with a non-empty draft
the arrow stays and Enter queues. Disabled = 40 % opacity. **There is no
attach button** — images enter by paste and document-level drag-drop (a
full-window overlay "Drag images here to add them / Up to {n} images, {size}
each").

**Keymap** (registered above the editor's defaults):

| Key | Behaviour |
| --- | --- |
| Enter | submit (queue while busy, per preference) |
| Shift+Enter | newline, unconditionally, decided *before* the IME guard |
| Ctrl/Cmd+Enter | "accelerated" submit = the *opposite* of the busy-Enter preference (Queue ↔ Steer); with an empty draft while queued rows exist it steers the whole queue |
| Enter (held) | swallowed (`event.repeat`) |
| Enter / Tab with a menu highlight | pick the highlight (Tab also drills folders) |
| ↑ / ↓ | menu navigation when open, else caret |
| Space | adjudicates a `/command` claim (the claim token carries its own trailing space) |
| Esc | closes an overlay first; a *claimed* command does **not** release — backspace the token |
| Paste | files → image intake; text → sanitized insert |

IME guard: `isComposing || keyCode == 229 || < 10 ms since compositionend`.

**Queue vs steer** (`submission-policy.ts`): if not running → send. Otherwise
`busyEnter` preference (Settings › General "Enter behavior while busy":
Queue | Steer) decides plain Enter; accelerated inverts it.

**Dock stack** (`conversation.input.dock`, full-width cards *above* the
card, gap 6, in order): **To-dos** (order 0), **Goal** (10), **Queue** (20).
The queue dock is r=`12 12 0 0`, `tip` fill, tucked 3 px under the card so
it reads as one surface; 1 row renders bare, >1 collapse behind
`[queue icon] {n} queued messages [chevron]`; per row: preview + hover
actions Edit (inline single-line input, Enter saves / Esc cancels) · Remove
· Steer (disabled unless running). Pending local echoes render as extra
inert rows.

**Under the card** (`composer.dock`): the **StatsLine** — one centred 13/20
`label[2]` line, groups joined by ` | `: `{turns} turns · {steps} steps |
LLM {d} · Tool call {d} | TTFT avg {d} · {tps} tok/s | Cache hit {p}% |
Input {n} tok · Output {n} tok`, ellipsized with a tooltip of the full line.

**Context ring** (`ContextMeter`): 14 px SVG ring r=5.5 stroke 2, track
`border-l3`, fill `label[2]`, monochrome at every percentage (**no threshold
colour**); tooltip `{percent} of context used`; click → a 264 px r=12 panel
with `~{used} / {window}`, a 4 px segmented bar (System prompt `bluish-400`
· Tools `rgb(167,139,250)` · Messages `blue-450`) and a legend with `~N`
counts. Renders nothing until capacity is known.

**Notices:** an `info` notice is a persistent r=8 `hover`-fill strip above
the card (12/18); errors and image rejections are toasts anchored to the
card.

**Composer takeovers** (`conversation.composer` chain): approval and
user-questions **replace the whole composer stack** in place — the
transcript stays scrollable above. See §6.

### 5.2 Port (**M**; dock **S**; takeovers in §6)

`input_bar.rs` becomes `ui/chat/composer/{card, toolbar, keymap, dock,
queue, stats, context_ring}.rs`.

- **Card:** `kit::elevated_frame(Soft, L2)` with r=22, `input_major` fill;
  `TextEdit::multiline` with `desired_rows` from the laid-out draft clamped to
  14 lines (336 px at 24 px), inner `ScrollArea` past that; the existing
  `wrapped_rows` helper stays. Hover/focus no longer changes the fill or
  stroke — dsh's card is static; focus shows only through the caret
  (`visuals.text_cursor.stroke.color = business`).
- **Toolbar:** replace `📎` with the `+` circle that opens the `/` menu
  with an empty query (the slash palette already handles `""`). Left:
  `permission_chip` (§6.7), `plan_chip` when `plan_active` (§6.5). Right:
  `model_select` (§6.8), `context_ring`, send/stop. Send/stop is one 34 px
  circle: `info_fill`, arrow glyph; `Stop` when `turn_in_flight && draft.is_empty()`,
  disabled-grey "Stopping" once `interrupt_requested`. Attach stays
  available through paste (exists) and drop (exists) plus the drop overlay
  (`Area` over the conversation column while `ctx.input(|i| !i.raw.hovered_files.is_empty())`).
- **Keymap:** add the `busy_enter: Queue | Steer` setting (default Queue);
  Ctrl+Enter inverts; empty-draft Ctrl+Enter with queued rows sends
  `SteerTurn` for each queued text (needs `Request::SteerQueued` or the FE
  looping `SteerTurn` + `Request::DropQueued` — see §11). Swallow
  `Key::Enter` repeats via `i.events` `repeat: true`. Esc order: slash menu →
  interrupt (exists).
- **Dock:** `dock::draw(app, ui)` renders in order the todo card (§6.6), the
  goal bar (§6.4), the queue dock. The queue needs the BE to *expose* the
  inbox: `Event::InboxChanged` carries only a count today. Add
  `Event::QueueChanged { session_id, rows: Vec<QueuedDump { id, text,
  images: u32 }> }` and `Request::EditQueued { id, text }` /
  `Request::RemoveQueued { id }` / `Request::SteerQueued { id }` (§11).
  Until then the dock shows the count and the last text the FE sent.
- **Stats line:** FE-side fold of `TurnUsage` events (§3.5) per session;
  `turns/steps` from `TurnStarted`/`ToolCallStarted` counts.
- **Context ring:** the status-bar meter moves here. Paint with
  `Painter::circle_stroke` for the track and an arc polyline for the fill
  (the current `working_sweep` already draws arcs). `TokenBreakdown` is on
  the wire now — the panel is the first thing that shows it. Compaction in
  progress: reuse the ring with a rotating arc and the tooltip "Compacting
  context…"; the `⟳ COMPRESSING` status text goes.

### 5.3 Attachments — the rail, files, the lightbox — **mostly done** (UI-8)

**dsh** (`ui-attachment`, `client/file-upload`): one ordered **draft rail**
under the text, non-wrapping, horizontal; edge arrows page the overflow, the
scrollbar is hidden, a new item scrolls into view. Every item is **64 px
high**: an image is a 64 px square thumbnail; a generic file is a **240 ×
64** card, r=16, blue-gradient document glyph, filename, then `EXT · size`.
Uploading swaps the glyph for a spinner with byte progress (an indeterminate
bar before the first report); failure shows **Retry**; the remove control
appears on hover or focus (always on touch). Clicking an image opens the
original. The limits (harness §9.6) are the drop overlay's second line. In
**history** a message renders files and images right-aligned in source
order: a lone image at 240 px on its long edge (aspect clamped to [0.25, 4],
never upscaled); with more than one attachment every image is a 64 px square
beside 240 × 64 cards, wrapping. A loaded image opens the document-level
**lightbox** (Esc, mask press or × close; focus returns to the opener); a
failed load shows a retry control. The same gallery serves the Trajectory
view and tool results that carry images.

**sica-rust:** images by paste, drop and the `+` picker into
`pending_images`, drawn as thumbnails; the drop overlay (UI-3); no generic
files, no lightbox, history images at a fixed size.

**Port:** the rail as a horizontal `ScrollArea` of 64 px items with the two
card shapes; `attach_from_path` accepts text files (`txt md csv json log
toml yaml rs …`, size-capped) as file cards that send as `@path`-style
`ContextInjected` text (harness §9.6) rather than bytes; history sizing per
the rule above; the lightbox is a `kit::modal` showing the image at
`min(viewport − 64, natural)` with Esc / mask / × and a Copy action; no
upload progress — the bytes are local.

**As shipped.** History sizing follows dsh's rule and `history_image_size`
is where it lives, with tests: a lone image is the message's subject and
gets **240 px on its long edge**, but is **never upscaled** — a 32 px icon
blown up to 240 is a blurry lie about what was attached; several images are
a set rather than a subject, so each is a 64 px square; and an aspect
beyond `[0.25, 4]` is clamped, which is what stops a panorama from becoming
a hairline nobody can click. Clicking one opens the **lightbox**, a
document-level `kit::modal` at `min(viewport − 64, natural)`, closed by
Esc, the mask or ×; Copy puts the image on the clipboard as a `data:` URI,
which is the form that pastes somewhere useful.

**Text files are references, not bytes.** `txt md csv json log toml yaml
rs py ts sql` picked or dropped into the composer become `@path` in the
draft — relative to the session's folder when they are under it — and the
backend expands them the way it already expands a typed `@path`. That is
one mechanism instead of two, and it keeps a megabyte of CSV out of the
session log.

Left here: the **240 × 64 file cards** and the paging rail. With text files
entering as references there is nothing pending to draw a card *for* — a
card would need generic file attachments to exist first, which is harness
§9.6's other half.

---

## 6. Control plane surfaces

### 6.1 Approval

**dsh** (`ui-approval/ApprovalPanel.*`): a **composer takeover**. Card
max-width = content width, **1 px `warn_secondary` border** (state borders
stay 1 px), r=20, `shadow-lv2`; a `warn_tertiary` strip with an 8 px warn dot
and **"Waiting for approval"** (13/18 `warn`); body (max-height 336, scrolls)
with the headline 15/24 500 = the reason, else "Tool {name} requests
privileged execution", and the command in 13/20 mono `label[2]`; action row
right-aligned: **Reject** (outline, hover `hover_danger` + `error` text) ·
**Allow once** (primary). Exactly two options, **no countdown rendered, no
keyboard accelerators**; the request carries an abort signal and unmounts on
cancel. The command text is fetched from the already-streamed tool row by
`callId` rather than duplicated on the request.

**sica-rust:** `draw_approval_strip` — caps "APPROVAL", `{skill} — {reason}`,
`args_preview`, `Allow once` / `Deny` ghost buttons, "no answer in 5 min
denies automatically".

**Port (S):** when `pending_approval.session_id == session_id`, `composer::draw`
renders `approval_panel` **instead of** the card. Keep the 5-minute note
but as the tooltip of the strip, not a line (dsh renders no timer; the BE
deadline is real, so hint it quietly). Button labels: "Reject" / "Allow once".
No protocol change.

### 6.2 User questions and plan review

**dsh** (`ui-user-questions/QuestionComposer.*`): also a takeover, **one
question at a time**: header `[eyebrow] [h2 question] [collapse] [×]`; body
with markdown detail and options as a radiogroup (numbered badges 1, 2, 3…;
single-select **auto-advances**) or checkboxes (multi); a "(recommended)"
suffix is stripped and rendered as a **"Recommended"** badge; free text is
always available (auto-growing 1-row textarea, "Type your answer", Enter
continues / Shift+Enter newline); footer pager `◀ {i}/{n} ▶`, **Skip this
question**, **Next** / **Submit**. Collapse keeps a header strip; × =
"Dismiss all questions" (cancels the call). Drafts persist per session. No
timeout rendered.

**Plan review** (`PlanReviewPanel`): the same takeover when the question is
the `exit-plan-mode` review — strip **"Plan review"**, body = the plan as
markdown, footer **Chat about it** (ghost + pencil, cancels) · **Refuse**
(outline) · **Approve** (primary).

**sica-rust:** a centred `egui::Window` "❓ Answer needed" with plain
buttons and a single-line field.

**Port (M):** `question_panel.rs` as a takeover. `QuestionAsked` carries one
question + options; add `detail: Option<String>`, `multi: bool`, `header:
Option<String>` and `intent: Option<QuestionIntent { PlanReview { approve_label } }>`
(§11) so the FE can pick the plan-review layout; a batch of questions can
stay one-at-a-time by the BE issuing them sequentially (it already blocks
per call). Numbered badges: 20 px r=999 `hover` fill, 12/500. Recommended:
strip the `(recommended)` suffix, render `kit::pill("Recommended")`, but
**send the original label** (dsh does; the model asked with it).

### 6.3 `/` and `@` menus

**dsh** (`ui-input-trigger`): `/` opens only at start-of-draft, after
whitespace, or after punctuation (`//` and `:/` are dead); `@` anywhere,
quoted tokens may span spaces. Sources: **Commands** (order 0, fuzzy
subsequence scorer with boundary/adjacency bonuses; free-input commands only
when the token is *leading*), **Skills** (order 2, prefix match, `user-only ·
…` prefix for non-model-invocable), **@ Files & folders / Sessions** (host
RPC, files first). Menu: anchored inside the card, `bottom: 100% + 4px`,
edge-to-edge, r=20, `menu` fill, elevation-prominent, **max-height 320**
clamped to the space above; rows min-h 40 r=10 14/22 `[kind icon] [name]
[description] [Browse folder ⇥ chevron for directories]`; group title rows
in 12/16 `label[2]`; ↑↓ cycle across groups; stale-while-revalidate with two
skeleton bars only when a group has nothing yet; all-empty auto-closes;
outside pointerdown closes. Picking a command **claims** the token (`/name `
stays, a ghost hint renders after it: `hint.plan = describe your task to
generate plan`, `hint.goal = describe the objective for a long-running task`,
`hint.goal.active = goal active — edit / pause / resume / clear`). A
**popupSelect** (search field, ↑↓, Enter, Esc; rows `label + detail +
check`) serves `/permission` and `/model`.

**sica-rust:** `slash_menu.rs` ranks with the dsh fuzzy scorer, groups
Commands → Skills → Agents, has ↑↓/Enter/Tab/Esc, and rewrites the draft to
`/name `. Both menus float in one shared `Area` (`slash_menu::overlay`)
pinned by its `LEFT_BOTTOM` pivot 4 px above the composer card — the card's
rect from last frame, since the menus are drawn first to claim the keys — so
the transcript no longer moves when the list opens or resizes, and a
pointerdown outside the menu closes it. The ghost hint is painted at the
caret from the galley's own cursor rect: `/goal `, `/permission ` and
`/agent ` carry the strings above, and any other claimed row shows its
declared `<args>`. `/plan` has no hint because it toggles on accept rather
than claiming the token. An **AGENTS** row is the one exception to "rewrite
the draft": picking one is a *selection*, so it sends
`Request::SetSessionAgent` and clears the draft (guide §5.2), and the choice
shows as a business-tinted composer chip next to the plan chip that clears
it on click.

**Port (S–M):** move the list into an `Area` anchored 4 px above the card,
r=20, max-height 320; rows to 40 px / r=10; kind icons; swap the ranking
for the dsh fuzzy scorer (`+8` at name start / after `-` `_`, `+4`
adjacency, penalties for skips — 30 lines). Add the ghost hint after a
claimed command (paint `label[3]` text at the caret's row end). `@` files:
FE-side walk of `workspace_root()` honouring `.gitignore` via the `ignore`
crate (the BE `glob` skill uses it already — or add `Request::ListFiles {
query }`), inserting the relative path. `@session` → §6.12.

### 6.4 Goal

**dsh** (`ui-goal/GoalBar.*`): a dock row (order 10): `[goal icon 14] [phase
label] [objective] [error?] [actions]`. Phase labels **"Ongoing Goal" /
"Paused Goal" / "Blocked Goal"**; hidden when complete or absent. Icon
actions: Pause (active) · Resume (paused) · Edit (inline input, Enter/Esc) ·
Clear. **No rounds, no complete/block button, no armed indicator** — those
are host-side. All verbs are compare-and-set on `{id, revision}`; failures
render inline as `{message} ({code})`.

**sica-rust:** `draw_goal_strip` — `GOAL RUNNING/PAUSED/DONE/BLOCKED` caps,
`round n/m`, objective, Pause/Continue/Complete ghost buttons.

**Port (S):** dock row per above; keep **rounds** as the tooltip on the
phase label (sica-rust surfaces them for a reason) and keep **Complete** as
a menu item under a ⋯ rather than a bar button. Edit needs `RunCommand
{ name: "goal", input: "edit <text>" }` (BE `/goal edit` — tiny). Armed vs
not: dsh has no indicator; sica-rust's "PAUSED while active-but-disarmed"
mapping stays, since pressing Stop disarms and the label must not lie.

### 6.5 Plan mode

**dsh** (`ui-plan/PlanModeControl.*`): entered only via `/plan` (or `/plan
<message>`), exited via `/plan off` or the chip. The chip lives in the
toolbar's plan seat **only while on**: r=999, pad 2 8, `warn_tertiary` fill,
`warn_label` text, 13/500, label **"Plan"** + a 12 px × glyph; tooltip "Plan
mode on — click to turn off (/plan)". No off-state toggle.

**sica-rust:** `PLAN ON` / `PLAN OFF` ghost toggle in the plan/todo row.

**Port (S):** `plan_chip` in the toolbar, shown only when `plan_active`;
click → `Request::SetPlanMode { active: false }`. Entering stays `/plan`.

### 6.6 To-dos

**dsh** (`TodoPanel.*`): dock card (order 0), 0.5 `border-l1`, r=12, `tip`
fill, pad 6 12; header button **"To-dos"** + progress `{done} completed ·
{active} in progress · {pending} pending` + chevron, **collapsed by default**;
items with 14 px glyphs: pending = dashed ring, in-progress = spinning
blue gradient ring, completed = solid ring + check. Empty → nothing.

**sica-rust:** `○ ◐ ●` glyph list in the plan/todo row.

**Port (S):** `todo_card` in the dock; paint the three glyphs with
`Painter` (dashed ring = 8 dashes; spinner = arc rotating on
`input.time`). Data is already `TodosChanged`.

### 6.7 Permission presets

**dsh:** a toolbar chip `[shield glyph 16] [label] [chevron]` (h=28, r=24,
13/20 500 `label[1]`, collapses to icon-only under 460 px): **Read Only**
(shield + check) · **Workspace Write** (filled shield + pencil) · **Full
access** (shield + `!`). Opens a `Menu` upward with the current item
checked; switching runs `/permission <id>`. Choosing Full access opens a
`RiskConfirmation` modal: "Enable Full access?", body about reduced
confirmations, a mandatory checkbox "I understand the risks and want to
continue", **Cancel / Enable Full access**. A second copy of the same gate
lives in Settings › General "Permission — Choose the default permission mode
for new sessions". No colour per preset.

**sica-rust:** `PERM WORKSPACE-WRITE` caps in the status bar, right-click
context menu; `default_permission_mode` has no UI.

**Port (S):** `permission_chip` → `kit::menu` → `Request::SetPermissionMode`.
Settings › General gets the default-mode row (writes
`default_permission_mode`, which `SessionCreated` already applies).
**Deviation:** the `RiskConfirmation` modal is *not* ported. dsh guards a
hosted, shared server; the desktop app's Full access reaches exactly what
the user's own account already reaches, and a modal that always answers
"Enable" trains the reflex that makes the next one useless. The consequence
is spelled out where the choice is made — the menu item and the Settings row
carry it — and switching back is one click. `kit::modal` stays in the design
system for the next dialog that earns one.

### 6.8 Model selection

**dsh** (`ui-model-selection/ModelSelect.*`): toolbar chip `[model name]
[effort in label[3]] [chevron]`, h=28 r=24; a two-level upward menu (r=20,
pad 4, `min-width 240`, `max-height 360`): root cells **Model** / **Effort**
(effort only if the model reports reasoning), model pane grouped by
provider with sticky group titles 12/18 `label[2]`, `menuitemradio` rows
min-h 38 r=10 14/20 500 with a trailing check; per-provider load-failure
strips with **Reload**; Esc backs out one pane. Shows the **name only** —
context window lives in the ring's panel. Picking a model also makes it the
default for new sessions; a session that has sent a request keeps its own.
Deleted default → chip reads "Select model" and the composer blocks with
"This model is unavailable — select one to continue" while the chip stays
live.

**sica-rust:** connect/disconnect lives on each provider card in Settings ›
LLM; the status bar shows the LLM dot + model name.

**Port (M):** `model_select` chip listing `llm_providers::load_all()`
grouped by provider title; picking → `App::connect_provider` (exists), which
already persists `last_active_provider`. Effort pane = the provider's
`thinking` toggle (`LlmOptions.thinking`) as Default / On / Off. The
"Ready/Connecting/Error" state that used to live in the status bar becomes
the chip's leading dot (blue chase while connecting, red on error with the
message as tooltip). Blocked composer when `llm_state` is `Disconnected` or
`Error`: placeholder "No model connected — select one to continue", every
control inert except the chip (replaces `draw_no_llm`).

### 6.9 Jobs

**dsh** (`ui-jobs/JobListAction.*`): a **session-header action**, not a
dock row, rendered only when the session has ≥1 job: trigger `[green dot if
any live] {n} background jobs running [chevron]` → a popover `<ul>` of rows
`[dot] [kind] [label] [detail | status] [duration]`; status words
running / stopping / completed / **cancelled** (for killed) / failed;
durations tick every 1 s only while open. **Read-only — no kill button, no
output viewer**; output surfaces through the transcript's tool rows.

**sica-rust:** `draw_jobs_strip` — `JOBS cli-3 [running]` + `Kill`.

**Port (S):** header popover per above; keep **Kill** as a row action (the
harness has `job-kill` and a human door matters) and add "Show output"
which sends `RunCommand { name: "job-output", … }` — today only the model
can call `job-output`; a `RunCommand` route for it is a 10-line BE change.

### 6.10 Schedule, subagent lineage, feedback, deliverables

dsh renders a **schedule** popover (header action, "Scheduled"/"Overdue",
"Every {n} {unit}") — sica-rust has no schedule (§12.8 of the harness
guide); skip until it exists. **Subagent lineage** (`ui-subagent`): a
breadcrumb dropdown tree of child *sessions* with per-node tokens and
active duration, hover-open 150 ms, keyboard tree nav; a subagent is a
separate session in dsh, whereas sica-rust's `subagent`/`ralph`/`agent-team`
children are `runner` conversations that never become sessions — render
them as **nested tool rows** (§3.4) with the child report as the body, and
put `UNVERIFIED` (which dsh does not have) as an amber `StateDot` +
"unverified" summary on the row. **Message feedback** thumbs and
**Produced files** chips are **n/a** for now (no feedback store; produced
files need `write-file` path collection at `TurnEnd` — **S**, worth doing:
the tail row `Produced [chip] [chip] + 2 files`, click opens in Explorer).

### 6.11 Workflow runs and the agent-team panel — **done** (UI-8, protocol v27)

**dsh** (`ui-workflow-run`, the experimental `client-ui-agent-team`): a
top-level workflow run is its **own conversation node**, rebuilt from four
durable events (`tool-workflow/run-start`, member start, member end, run end)
and independent of the workflow tool card. The run is a 32 px disclosure row
`[chevron] [state dot] [title] [status]`; **phases** are disclosure rows with
title + member count and a fixed aggregate-status tail; **members** are rows
with a 16 px dot slot, a truncating name and a fixed status column. A mount
opens running / failed / cancelled / interrupted levels and closes completed
ones; the first abnormal edge opens once, normal completion closes once
(delayed while focus is inside), and a new running member under a completed
phase reopens it. A **running** member whose child session is in the
ordinary list, is `origin: subagent` and is parented by this session is
clickable and opens the child; everything else is inert. A turn that closed
with missing terminal events shows the run as **interrupted** without
touching the tool result. The agent-team panel is the same shape for a team:
one row per teammate with its report state.

**sica-rust:** `workflow` (harness §12.5) and `agent-team` render as a
**Delegate** tool row (§3.4) whose nested children are the live
`ToolCallStarted` rows; `phase()` / `log()` are `LogLine`s, so history shows
the row and the printed result only.

**Port (M, gated on harness §12.5 "left for later"):** the durable
`WorkflowRun` events are the prerequisite — `EventKind::WorkflowRun {
run_id, phase: Option<String>, member: Option<String>, state }` written on
run start, member start / end and run end, so a reload rebuilds the tree the
live events built. With them the Delegate row's body becomes the run → phase
→ member tree with dsh's open/close rules (`disclosure_row` + `StateDot`),
and the `[id: call-N]` citations of a team report (harness §12.2) link to the
member rows. Until then, promote the `phase()` `LogLine`s into the row body
as a phase list — one line of FE work that already makes a long run legible.

**The interim shipped.** `workflow` and `agent-team` prefix every progress
line with their own name, and only one such call runs at a time in a
session — both cap their children and drive them from a single dispatch —
so `App::note_on_running_chip` routes those lines onto the newest
unfinished chip of that name, and the row body lists them (last twelve,
capped at 200 kept, so a script that logs in a loop cannot grow the
transcript without limit). A *running* orchestrator now shows its phases
where "running…" used to be, which is when a run spending thirty child
conversations most needs explaining.

It is **live only**: a reload rebuilds the row with its result and no
phases, because nothing about the run reaches the log. That is exactly what
the durable `WorkflowRun` events would fix, and they are still the
prerequisite for the full run → phase → member tree.

**The fork was settled: four events.** `EventKind::WorkflowRun { run_id,
call_seq, phase, member, member_id, state }` is written on run start, member
start, member end and run end. The deciding case is the run that did not
finish: with four rows an interrupted run is visible *because* its terminal
rows are missing, where a summary written at the end would have left nothing
behind at all — and that is the run a reader most needs to see. The rows are
**non-surface**, so a workflow's children stay out of the model's history;
only what the script printed reaches it, exactly as before.

`agents::workflow` reports edges through `RunNotifier`, the same split
`JobNotifier` makes for background jobs — `agents` knows how to run a
workflow and nothing about where its history lives. `backend::chat::
WorkflowBridge` appends each edge and pushes the **whole run**, rebuilt from
the log by `sica_core::project::workflow_runs`, so the live tree and the one
a reload draws cannot drift: both are folds of the same rows.

The body renders run → phase → member with dsh's open/close rule — a phase
that is running or holds a failure stays open, a wholly finished one closes,
so a completed run collapses to a line and a broken one does not make the
reader go looking. A run whose rows stop while it still says *running*, with
nothing running under it, reads as **interrupted**.

The phase list from the interim is still there and still useful: it is what
`agent-team` shows, since only `workflow` is instrumented, and it is what a
run shows before its first member starts.

### 6.12 `@session` references — **done** (UI-8)

**dsh** (`ui-reference`, `dsh-session-reference`): the `@` menu is one list
— **Files & folders** first (a directory row carries **Browse folder** on
Tab or its chevron and keeps the menu open at the trailing slash), then
**Sessions** (`title · cwd · age`, the workspace named only when it is not
the current one). A session pick inserts an atomic chip (chat-bubble glyph +
title) whose serialized form is `@[label](dsh-session:<id>)`; on send the
host validates the mention and captures the referenced session's context at
the pre-step boundary inside the untrusted frame, and a failed capture ends
that turn. `@"…` searches files only.

**sica-rust:** the `@` picker (UI-6) lists files and drills folders; no
sessions.

**Port (S FE + S BE):** a **Sessions** group under the files, fed by
`App.sessions` (title, the `~`-shortened `cwd` from `SessionMeta.cwd`,
relative age), inserting `@session:<id>`; the BE's `send_user_message`
resolves the token to `ContextInjected { source: SessionReference(id),
content }` — the referenced log's derived transcript through `retain` (head +
tail, 8 KiB) wrapped in `UNTRUSTED_NOTICE` (harness §9.4) — and refuses an
unknown id with `Response::Error` before the turn opens.

**As shipped, with one softer edge.** The picker lists **Sessions** under
the files — title · `~`-shortened cwd · age, newest first, the current
session excluded because a session cannot usefully reference itself — and
inserts `@session:<id>`. The backend resolves it in `start_turn`, *before*
anything of the turn is written, into `ContextInjected { source:
SessionReference { id } }`: the referenced log's derived transcript through
`retain` (6 KiB head + 2 KiB tail) inside the untrusted frame.

The reference is resolved to **content, not kept as a pointer**. The
referenced session goes on changing, and a prompt that meant one thing when
it was sent must not mean something else when the log is replayed.

The softer edge: an unknown id is a `LogLine` and a skipped reference
rather than a refused turn. dsh ends the turn, but here that would cost the
user their whole message over a typo in one token — and the turn without
the reference is still the turn they meant to send.

---

## 7. Settings

### 7.1 What dsh does

`ui-settings*`. Trigger in the sidebar foot; opens a **modal**: full-window
mask `rgba(0,0,0,.24)` + 2 px blur; panel **800 wide × min(800, vh − 48)**,
r=32, `bg_layer[1]`, elevation-prominent. Left nav rail 188 px (pad 22 12 0,
title "Settings" 16/24 500, cells h=40 r=12 14/22 with icons, hover
`sidebar_hover`, active `sidebar_active`). Content column: header h=54 with
the section title, **"Open configuration file"** (outline sm) and a 28 px
close; body pad 0 24 24, scrolls. Esc, mask click, × close; focus returns to
the trigger; `activeId` resets on close.

Sections by order: **General** (0) · **Models** (10) · **Plugins** (15) ·
Agent presets (20). Rows share one rhythm: pad 16 0, 0.5 `border-l2`
bottom, title 14/22 `label[0]`, description 12/18 `label[2]`, control right.

**General rows** (by order): Permission (−20, "Choose the default permission
mode for new sessions", selector + risk gate) · Language (0) · **Appearance**
(10; three "cubes" `flex 1 1 180px`, pad 20 32, r=20, 0.5 `border-l4`,
icon-over-label Light / Dark / System, selected = `bg-module-platform` fill +
`bluish-400` border) · **Font size** (11, "Only affects conversation
content", a stepper pill h=36 r=18 with hover-revealed −/+, 12–17) ·
**Conversation display** (12, "Controls process content in completed turns",
Normal / Compact) · **Enter behavior while busy** (20, "Busy only;
Cmd/Ctrl+Enter uses the other behavior", Queue / Steer). **General rows
apply live** — no Apply button.

**Models**: intro "Enter your API keys to use models from the following
providers."; one card per provider (`{Display} {id}` header, API key field
`password`, a **"Customized settings"** fold with Display name / Base URL /
API protocol / Models list with per-row Model ID · Display name · Context
window · Max output tokens), footer **Cancel / Apply** ("Applying…"), one
card open at a time; dashed **+ Add provider** / **+ Add a custom
provider** cards; **"Fetch available models"** ("Asking the provider…")
probes the endpoint *as typed* and opens a searchable "Choose models to add"
picker with Select all / Add selected. Validation is client-side with
positional messages (`Model 2: Model ID must be unique.`); writes are
revision-fenced ("Someone else changed these settings while this card was
open…"). Delete = confirmation dialog naming the provider.

**Plugins**: two tabs, **Plugin configuration** (cards: Shell — command
timeout / output cap; Agent loop — parallel tool calls; Subagent — allowed
models; Web search) with "Overridden" badges and "Reset to default", and
**Plugin list** (read-only inventory with Enabled / Disabled / Failed tags,
runtime status, search).

### 7.2 Port (**M–L**)

`settings::draw` → `kit::modal`-style panel (800 × min(800, h−48), r=32) with
a 188 px nav and a content column. Sections:

| Section | Rows / cards | Source of truth |
| --- | --- | --- |
| **General** | Default permission · **Working directory** (the folder the agent reads, writes and runs commands in — `paths::working_dir`, distinct from the app root that holds settings, sessions and skills; a picker menu over the app folder, the last five choices and a native chooser; changing it restarts the BE, which resolves its file skills once at startup) · Appearance Light/Dark/System cubes · Font size stepper 12–17 · Conversation display Normal/Compact · Enter while busy Queue/Steer · Startup (auto-start BE, auto-connect LLM) · Logging (raw LLM) · Idealist auto-apply | `settings_store::Settings` — **live apply** on change (`apply_and_save_settings` per change; drop the Apply bar) |
| **Models** | one card per `sica-settings/llm-providers/*.toml`: title + id, API key (password), "Customized settings" fold with Base URL · Model · Temperature · Max tokens · Context window · Native tools · Thinking · Compact policy (threshold / retain / summary tokens) · preset drift + Apply preset; footer Cancel / Apply; **Connect** stays on the card *and* on the composer chip; dashed "+ Add provider" creates a TOML from `llm::preset`; "Fetch available models" = `GET {base}/v1/models` via a new `Request::ListModels { provider }` (the BE holds the HTTP client) → picker | `frontend::llm_providers` |
| **Skills** (replaces dsh Plugins) | tab **Catalogue**: the `ListCatalog` entries grouped Commands / Skills / Agents with descriptions and source paths, search, "Open folder" per kind, skill-creator template button; tab **Harness**: shell timeout (`Skill::timeout`), output caps, `MAX_TOOL_HOPS`, parallel pool cap, compaction thresholds — **read-only until those are settings** (today they are constants); tab **Delegation**: `agent-team.md` on/off switch (rename to `.md.off`), child-excluded list | `agents` constants → later a `harness.toml` |
| **Agents** (dsh Agent presets) — **done** | roster cards from `agents/*.md` (harness §5.2): name · description · `skills:` count · **In use** when the current session runs it · **Set as default** (a `default_agent` setting the FE applies through `SetSessionAgent` right after `SessionCreated`) · **Show location** (`open_in_explorer`) · **Duplicate** (dialog "Copy preset · copied from {name}": *Identifier* → the new filename, placeholder `my-agent`, "Identifier is required / invalid / taken"; *Name*; Create) · **Delete** with a confirmation naming the preset; a file that fails to parse renders as a marked card with the reason as tooltip and only Show location / Delete live. Plus the **new-session chip** on the hero ("Agent preset for the session you are about to start", opens on the default, spent by the next session) and a read-only **header label** ("fixed when it started"). dsh's read-only shipped presets and its Creator-mode add-card have no counterpart: every sica preset is a file the user owns | `agents/*.md` + `settings_store.default_agent` — **S–M** |
| **Integrations** (dsh Plugins › Plugin configuration) — **done** | one card per optional integration, each read from its file and reporting what the BE said at startup: **MCP servers** (`sica-settings/mcp/*.toml`: name, command, an `enabled` toggle that rewrites the TOML, status Started / Failed with the `LogLine` reason, tool count from the catalogue) · **Hooks** (`.sica/hooks.json` under the session's cwd: the matched events, read-only, "Open file") · **Web search** (`web.toml`: provider select, key field **write-only** — shows "configured" after save, harness §14.6) · **Workflow / Agent team** switches (rename `skills/{workflow,agent-team}.md` ⇄ `.md.off`). dsh's "Overridden / Reset to default" pair has no meaning here: a card *is* its file | the files named — **S–M**; an MCP change needs the BE restarted, which the row's `applies: restart` badge says |
| **Diagnostics** (new home for Communication) | connection card (BE pid / IPC / protocol version / build id, Start / Stop / Rebuild & Restart, auto-watch, release profile), the log panel with a level filter, the demo request row | `controls.rs`, `log_panel.rs` |

The "Open configuration file" header action opens `sica-settings.json` in
the OS editor (`open_in_explorer` exists).

**Integrations, as shipped.** Every integration this app has is a file that
is absent by default, so each card *is* its file: it reads it, writes it and
can open it. Status is **derived rather than asked for** — the frontend and
backend share a machine, so the configuration half is read straight from
disk, and the live half comes from the catalogue the backend already sends:
an MCP server that started has its tools in it under `mcp__<server>__<tool>`.
That saves a request and cannot disagree with what the model can actually
call, because it is the same list. Cards: **MCP servers** (command, an
`enabled` checkbox that rewrites the TOML line-wise so comments and key
order survive, tool count, and an unreadable file reporting its parse error)
· **Hooks** (the events `.sica/hooks.json` matches and how many commands
each has — never the commands themselves) · **Web search** (provider pills
and a **write-only** key: the card shows *configured · in file /
environment (VAR) / .env (VAR)*, and `${VAR}` is readable while typed
because a variable name is not a secret) · **Optional skills** (`workflow`
and `agent-team`, switched by renaming `.md` ⇄ `.md.off`). Rows that need
the backend restarted say so. The same write-only treatment now covers the
provider key in Settings › Models, which closes harness §14.6's other half.

Four tests cover what a screenshot cannot: that the toggle preserves a
hand-written file, that a file without the key gets one appended, that the
hooks line counts commands without printing them, and that **no branch of
the key status can print the key**.

**Agents, as shipped.** One card per `agents/*.md`: description, how many
skills its `skills:` list leaves visible, *in use by this session* when the
current session runs it, and the actions that make sense for a file the
user owns — Show location, Duplicate, Delete (armed, as everywhere else),
and Set as default. A file that fails to parse **still gets a card**: it is
on disk and hiding it would leave the user with a preset that appears
nowhere and does not work, so the card carries the parse error and only
Show location and Delete are live on it. dsh's read-only shipped presets
and its Creator-mode add card have no counterpart, as this guide already
said: every preset here is a file the user may edit.

The Duplicate dialog validates the identifier *before* anything is written,
because the identifier is the filename — required, filename-safe, not
already taken — and the copy's `name:` line is rewritten so it is
addressable as itself rather than as a second file claiming the original's
name. `default_agent` is a setting the frontend applies through
`SetSessionAgent` right after `SessionCreated`, which is the only moment it
can be set: a session's preset is fixed once the session has produced
anything (harness §5.2).

The **hero preset chip** and the read-only **header label** are in too.
The chip applies its pick to *this* session rather than staging it for a
later one, which is the same reasoning as §4.3's workspace picker turned
around: a session's preset is fixed once it has produced anything
(harness §5.2), and the chip only appears on an empty session — so now
is exactly when it can still be set. The header label is deliberately
not a control: it names what the session is fixed on, and a control
there would fail on click.

Not shipped from this section: General's Working-directory relabel
(harness §3.9's leftover — it needs a request that sets the default on a
live backend).

### 7.3 Onboarding — **done** (UI-8)

**dsh:** on a first run with no usable provider the app root is made inert
behind a body-level stage: a **welcome** dialog (the preview notice,
**Continue**), then **"Add an API key to get started"** — "Configure the
official DeepSeek provider to start building.", one **API key** field,
**Configure later** / **Save and continue** (disabled until non-empty,
"Saving…"). Saving writes the credential and returns to a working composer;
"later" leaves the composer blocked with **Select model** (§6.8). The Models
list marks a provider with an **"API key missing"** badge; **Open
configuration file** sits in the settings header.

**sica-rust:** a fresh checkout starts disconnected with the §6.8 blocked
composer and no hint that Settings › Models is where to go.

**Port:** at startup, when `llm_providers::load_all()` has no provider with a
key, open a `kit::modal` over the shell: "Add an API key to get started" ·
provider select (the TOML stems, "custom" last) · API key field · **Configure
later** / **Save and continue** → writes the key into that provider's TOML,
sets `last_active_provider`, connects. The Models cards get the **key
missing** badge (`warn` `StateDot` + tooltip). No welcome dialog — the
preview badge on the hero already says what the build is.

**As shipped, with one deliberate departure.** dsh is a hosted product
where a key is required, so it makes the app inert until one is entered.
Here a local provider — vLLM, llama.cpp — needs **no key at all**, and
blocking a local-only user behind a key field would be nagging them for
something they must not supply. So: the dialog says a local server needs no
key, *Configure later* is a real answer that is remembered (`onboarded`),
and nothing is made inert — the composer behind it is the §6.8 blocked
composer, which already explains itself.

It offers itself only when **nothing has ever worked**: no provider carries
a key *that resolves* (a `${VAR}` pointing at an unset variable is exactly
the state it is for, §14.6) and no provider was ever connected — having
connected once is proof the user knows where the setting lives. Saving
writes the key into that provider's TOML and connects, because the user
came to make the app work rather than to fill in a form.

The **API key missing** badge follows the same rule: it appears only on a
provider whose base URL is not loopback, since an empty key on a local
server is the correct configuration and badging it would report a fault
that is not one.

---

## 8. Sessions header, title, workspace

Covered by §2.2 (header) and §4 (rows). Title generation already matches dsh
(fallback → LLM → user-pinned once `RenameSession` exists). Workspace: the
header's first crumb is the **session's own** workspace (`SessionMeta.cwd` →
its `WorkspaceDump.title`, or the folder name for an Ungrouped session),
tooltip = full path, click copies it — dsh's hover-card copy behaviour; the
picker and the grouping are §4.3. Until harness §3.9 lands the crumb shows
the app-wide `working_dir()`, as it does today.

---

## 9. Diagnostics, connection, toasts

**dsh:** connection indicator (§1.3 `kit::connection_indicator`) inline by
the Settings trigger: **silent while healthy**, "Disconnected" / "Connecting…"
(dots advance every 500 ms) with hover text "Reconnect now" (click retries
immediately), "Connected" for 2000 ms after recovery. No banner, no host
version, no log viewer in the client. Toasts: top-centre over the
conversation, one at a time, caller-owned hold, no severity kinds; **inline
error text is the dominant pattern** (`role="alert"` spans inside cards).

**sica-rust:** three status-bar icons (BE / IPC / LLM) with tooltips, a
pulsing RESTART pill on version drift, a protocol-mismatch banner, and a
log panel that has lost the tracing level.

**Port (S–M):**

- `connection_indicator` in the sidebar foot fed by `ipc_state` +
  `be_state`: `Disconnected` when IPC is down or the BE exited, `Connecting`
  while spawning/handshaking, `Recovered` 2 s after `IpcConnected` if there
  was an outage. Click → `UiCommand::StartBe` (or restart). The **RESTART**
  affordance for source drift becomes a second, persistent chip in the same
  foot row ("Rebuild & restart", `warn_tertiary`), because dsh has no
  equivalent and the hot-reload loop is sica-rust's core feature — do not
  hide it. The protocol-mismatch banner becomes the same chip in `error`
  colours.
- **Preserve the level:** `supervisor::forward_event` maps
  `LogLine.level` to `LogKind::{Error, Warn, Info, Debug}`; add a level
  filter to the log panel and colour rows with `error` / `warn` / `label[1]`
  / `label[2]` instead of the hard-coded RGBs. WARN/ERROR lines from the BE
  also raise a **toast** (hold 3000 for WARN, 6000 for ERROR, `warn` icon)
  anchored to the conversation column — that is what makes a rejected tool
  call visible without opening Settings.
- Toast queue: dsh has none; give `App` an `Option<Toast>` — a new toast
  replaces the current one (bump `seq` so the fade restarts).

---

## 10. Trajectory view (**L**)

**dsh** (`ui-trajectory`): the second tab. Full-bleed on `bg_layer[0]`, a
32 px toolbar (Duration toggle "Use actual duration / equal-width", Turns
and Calls collapse-all, live search that dims non-matches), a **timeline**
strip (`Total {d} · Started {t} · TTFT {d} · Decoding {d}`, drag a range to
filter), and a **ledger**: fixed-layout table, 12 px, 30 px rows, columns
`# | kind tag | text | Input | Output | Think | Time`; kind tags r=6 22 px
tinted per kind (SYSTEM/COMPACTED grey, USER green, CONTEXT green-mix,
ASSISTANT accent-mix, TOOL amber, SUBTOOL dimmed amber + 28 px indent);
numbered **request boundaries** (`Request #n`, red when failed) with
per-request usage and a running cumulative; sticky 44 px turn headers
(`Turn n`); a row click opens a resizable inspector with tabs Summary ·
Payload · Result · Schema · Timing · Diff · Source · System Prompt · Tools ·
Options · Usage · Raw. History pages 50 nodes.

**sica-rust:** `sessions/<id>.jsonl` *is* this ledger, and the FE already
receives `SessionDump` as a derived surface — so the view's whole value is
that it shows the *other* thing: the log, including everything the fold
shadowed.

**Port:** `Request::LoadSessionEvents { id, from_seq, limit }` →
`Response::SessionEvents(Vec<EventDump>)` where `EventDump` is a
protocol-safe mirror of `SessionEvent` (seq, ts, kind tag, text, tokens,
surface op, tool ids). Render as a `TableBuilder` (`egui_extras`) with the
kind tags and turn headers; an inspector `SidePanel::right` (the details
column from §2) with Summary / Payload / Result / Timing / Raw tabs — Schema,
System Prompt and Tools tabs need the *request envelope*, which
`TokenUsage.breakdown` hints at but the log does not store; store a
`RequestEnvelope { system_hash, tools_hash, tokens }` on `TurnStart` first
(§3.3 of the harness guide's projections). The Inspect pill on tool rows
(§3.4) jumps here with `focus = call_id`.

**Built** ([ui/chat/trajectory.rs](../crates/frontend/src/ui/chat/trajectory.rs),
[backend/src/trajectory.rs](../crates/backend/src/trajectory.rs)). The
backend flattens each `SessionEvent` into an `EventDump` — a coarse kind tag,
one line of text, the payload/result bodies, the provider's own token pair,
the `ToolCall` join, the enclosing `turn_id`, and the event's JSON for the
Raw tab. Two fields do the work the transcript cannot: `shadowed` (asked of
`derive_surface` itself rather than re-derived, so the two never disagree)
and `shadows`, the span a compaction or a rewind covered. A shadowed row is
dimmed and struck through — still in the log, gone from the model's view.

Request boundaries come from the log rather than from a separate stream:
each `TokenUsage` closes a successful request and each `LlmRetry` closes a
failed one (a failed attempt persists no usage, so the retry row *is* the
boundary), which is what makes `Request #n — failed` truthful.

The **request envelope** deviates from the sketch above in one way that
matters. The guide put a `RequestEnvelope { system_hash, tools_hash, tokens }`
on `TurnStart`; what landed is a durable `EventKind::RequestEnvelope`
carrying the *bodies* — the composed system prompt, the `tools` array, and
the sampling options as JSON — appended by the hop that composed it and
**only when its fingerprint differs from the last envelope in the log**. Two
reasons. Hashes alone cannot fill a System Prompt tab, so the bodies have to
live somewhere, and the log is the only place that survives a restart, a
fork and a session reload alike. And the prompt is a property of a *request*,
not of a turn: plan mode can flip mid-turn and `memory.md` is re-read every
turn, so an envelope pinned to `TurnStart` would describe some of its own
turn's hops wrongly. Instead every row names the newest envelope at or
before it (`EventDump.envelope`), and the bodies travel once per page beside
the rows (`Response::SessionEvents.envelopes`) rather than once per row — a
session whose prompt never changes stores and sends exactly one copy. The
inspector's **Schema** tab cuts the selected tool's entry out of that
envelope's tools array, and says plainly that a text-protocol request sent no
schemas at all rather than showing an empty box.

The ledger is painted rather than built on `egui_extras::TableBuilder` — a
fixed-width table did not justify a new dependency — turn headers scroll
instead of sticking (egui has no sticky row), a timeline segment click
scrolls to that turn instead of drag-filtering a range, and there is no
**Think** column: nothing durable carries per-event reasoning tokens
(`Event::TurnUsage` does, and is never logged), so the reasoning body goes
in the inspector's Result tab rather than a token column filled with a
different unit.

---

## 11. Protocol impact (one bump, v17)

Additive only; `#[serde(default)]` on every new field so old logs load.

| Change | Why | State |
| --- | --- | --- |
| `Event::ToolCallStarted += args_json: String` · `ToolCallFinished += output: String, duration_ms: u64` | tool row body (IN/OUT, diff, terminal) §3.4 | ✅ |
| `EventKind::ToolCall += args_json` (durable) | a reloaded row needs the real args, not the truncated preview | ✅ |
| `MessageDump += tool_call_id, tool_parent_id, tool_depth, tool_args_json` | history rows regain the tree | ✅ (flat: `tool_call_id` is the `ToolCall` seq, and only top-level calls reach the log — nested `SkillContext::sub` calls are live events only, so `tool_parent_id` is always `None` on reload) |
| `Event::LlmRetry { session_id, attempt, max, delay_ms, reason }` | retry chain row §3.5 | ✅ |
| `Event::TurnUsage { session_id, turn_id, prompt, completion, reasoning, duration_ms, ttft_ms }` | tail pills, stats line §3.5 / §5 | ✅ (`turn_id` is the backend's *outer* turn; the FE opens one row per hop, so the pills land on the session's last row — the one carrying the final answer) |
| `SessionMeta += updated_at: i64` | Last-updated ordering, relative time §4 | ✅ (unix **seconds**, like `created_at`; the log's own `ts` is milliseconds) |
| `Request::RenameSession` · `ForkSession` · `ArchiveSession` · `SearchSessions { query }` → `Response::SessionSearch { hits }` | row menu, search §4 | ✅ (`EventKind::SessionArchived` is the durable flag; `SessionLog::fork` copies seqs verbatim so `ToolResult.call_seq` joins survive) |
| `Event::QueueChanged { session_id, rows: Vec<QueuedDump> }` · `Request::EditQueued / RemoveQueued / SteerQueued { id, … }` | queue dock §5 | ✅ (v19; `QueuedDump.id` is an inbox-minted handle, not a log seq — a queued message has not been logged yet, and a *position* stops naming the same row the moment the loop claims one. Paired with `InboxChanged` on every change and pushed alone on session load. A verb naming a row that already ran answers `Response::Error`; steering a row with images is refused rather than dropping them) |
| `QuestionAsked += detail, multi, header, intent` | question / plan-review takeover §6.2 | `detail` + `multi` ✅ (v21; `ask-user` gains the matching optional args, and a multi-select answer crosses back as the ticked labels joined with `; ` — one string, so nothing downstream changes). `header` / `intent` **not ported**: the takeover already reads the plan review off the question itself, and a second, redundant signal for it would only be a way for the two to disagree |
| `Request::RunCommand` accepts `goal edit <text>` and `job-output <id>` | goal edit §6.4, jobs popover §6.9 | ✅ (`goal edit` is a separate door from `agents::goal::apply` — it rewords the objective and leaves the phase, the rounds spent and the arming alone. It is still a compare-and-set, and there is deliberately no `GoalAction` for it: a round that could rewrite its own objective could call anything it managed to do a success) |
| `Request::ListModels { base_url, api_key }` → `Event::ModelsListed` | "Fetch available models" §7 | ✅ — **deviation:** the guide sketched `Response::Models`, but the dispatcher loop is serial and a slow provider would stall every other request behind it, which `CLAUDE.md` forbids. It answers `Ok` and pushes the list as an event, like `ConnectLlm`. Provider configs live on the FE, so the FE sends what the call needs rather than a `provider` id. |
| `Request::LoadSessionEvents { session_id, from_seq, limit }` → `Response::SessionEvents { events, total, next_seq }` | trajectory §10 | ✅ (v20; `EventDump` is the flattened mirror — tag, one-line text, payload/result bodies, the provider's token pair, the `ToolCall` join, the enclosing `turn_id`, `shadowed` + the `shadows` span, and the event's own JSON for the Raw tab. Paged: the backend caps a page at 500 and answers `next_seq` when more remains) |
| `Event::ToolCallStarted += call_seq: u64` | the Inspect pill §3.4 | ✅ (v20 — **addition**: the live event carried only the process-local tool id, while a reloaded row carries the durable `ToolCall` seq, so the same call had two identities and the pill had nothing stable to jump to. `0` for a nested `SkillContext::sub` call, which is a live event only and never reaches the log) |
| `EventKind::RequestEnvelope { fingerprint, system, tools, options }` · `EventDump += envelope` · `Response::SessionEvents += envelopes: Vec<EnvelopeDump>` | the inspector's Schema / System Prompt / Tools / Options tabs §10 | ✅ (v22 — **deviation:** the bodies live in a durable log event written on *change*, not a hash on `TurnStart`; see §10. `EventTag::Prompt` is its ledger tag) |
| `TurnFinished.finish_reason` gains `"max_tokens"` and `"interrupted"` as stable strings | §3.2, §3.5 | ✅ (`run_turn` normalises the provider's `"length"`; `chat.rs` ends the turn on it rather than hunting for a tool call in a truncated reply) |
| `Request::ListWorkspaces` · `CreateWorkspace` · `RenameWorkspace` · `DeleteWorkspace` · `MoveWorkspace` · `MoveSession` → `Response::Workspaces { rows, ungrouped }` · `Event::WorkspacesChanged` · `NewSession { workspace_id }` · `SessionMeta += cwd` | workspace grouping, picker, add / rename / delete §4.3, header crumb §8 | ⏳ v26 — harness Wave 9 (§3.9) |
| `ContextInjected` source `SessionReference(id)`; `@session:<id>` resolved in `send_user_message` | `@session` references §6.12 | ⏳ log-only, no bump |
| `EventKind::WorkflowRun { run_id, phase, member, state }` | the workflow run body §6.11 | ⏳ log-only; harness §12.5 "left for later" |

`forward_event` keeps `LogLine.level` (no wire change). `PROTOCOL_VERSION`
is **22** (v17 batch → v18 prompt editing → v19 queue verbs → v20 trajectory
→ v21 `QuestionAsked.detail` / `.multi` → v22 the request envelope); `smoke`
passes on it, and now exercises `LoadSessionEvents` too — including the rule
that a session with nothing sent yet reports no envelope rather than
pointing at one.

---

## 12. Roadmap — five UI waves

Each wave is one commit series that builds, passes `.\run.ps1 test
--workspace`, and passes the `smoke` binary before the next starts.

| Wave | Scope | Size | Protocol |
| --- | --- | --- | --- |
| **UI-1 Foundation** ✅ | `Theme` v2 with the static scale + two alias maps, `apply_visuals`, `ui::kit` (button, icon_button, pill, state_dot, disclosure_row, menu, modal, toast, elevated_frame, hairline, code/IO blocks), three-column layout with the collapsible 280/56 sidebar, status bar removed → connection indicator + Rebuild chip in the foot, content width axis 680–920. **Deviations:** the UI face is the *platform* sans loaded at runtime (dsh's own rule) rather than bundled Inter, and the code face stays IBM Plex Mono; icons are painted by `ui::icons` instead of pulling resvg; elevation is one blur layer plus the 0.5 px hairline, since egui's `Frame` carries a single shadow. | **M** | none |
| **UI-2 Transcript** ✅ | user bubble with `/name`+`@path` runs, flat assistant, `TurnStatus` shimmer + 15 s clock, reasoning disclosure with the sweep glare, tool rows with variants/states/nesting and diff / terminal / read / search / IN-OUT bodies + the `+A -R` diff stat, the retry chain with its live countdown, per-turn usage and time pills on the tail, error / max-tokens / stopped / compaction / injection rows, compact-mode turn fold, hero, back-to-bottom. **Open:** nothing. Produced-file chips and the tail's branch action landed with UI-6; the Inspect pill landed with UI-5. | **L** | v17 batch 1 ✅ |
| **UI-3 Composer + control plane** ✅ | r=22 card, toolbar (`+`, permission chip + risk gate, plan chip, model select, context ring with the `TokenBreakdown` panel, send/stop), keymap with the busy-Enter preference, dock (to-dos, goal, queue with per-row Edit · Remove · Steer over the backend's real inbox), stats line, approval and question/plan-review takeovers, `/` menu with dsh's fuzzy ranking, drop overlay, toasts for WARN/ERROR. **Open:** nothing. `@file` completion landed with UI-6; the floating menu overlay and the claimed-command ghost hint landed with UI-7. | **L** | v17 batch 2 ✅ (queue dock landed on v19) |
| **UI-4 Settings + sessions** ✅ | Settings modal with General (live) / Models / Skills / Diagnostics; session rows with the status dot, relative time and a ⋯ menu (Open · Rename · Fork · Archive · Copy title · Delete); inline rename, Last-updated order, the header search field with dsh's 250 ms debounce over the backend content scan, and "Fetch available models" as pickable chips per provider. **Open:** nothing — un-archive stays deliberately absent (§13). | **M** | v17 batch 3 ✅ |
| **UI-5 Trajectory** ✅ | second tab over the event log; toolbar (live search that dims non-matches, collapse-all turns, actual-duration / equal-width); timeline strip (`Total · Started · Requests` + one clickable segment per turn); ledger with kind tags, turn headers, numbered request boundaries carrying per-request usage and a running cumulative, and **shadowed rows struck through** — the fold's leavings are the point of the view; the event inspector in the details column (Summary · Payload · Result · Timing · Raw); the Inspect pill on tool rows jumping to the call's own row. **Deviations:** no **Think** column (no durable per-event reasoning count exists — `Event::TurnUsage` carries one but is never logged; the reasoning body is in the inspector's Result tab instead); turn headers scroll rather than stick (egui has no sticky row); a segment click scrolls to that turn rather than drag-filtering a range; paging is a **Load more** button over the backend's 500-row cap rather than 50-node infinite scroll; the ledger is painted rather than built on `egui_extras::TableBuilder`, which would have been a new dependency for a fixed-width table. **Open:** nothing. The Schema / System Prompt / Tools / Options tabs landed with UI-6 on a durable `EventKind::RequestEnvelope` — see the deviation note in §10. | **L** | `LoadSessionEvents` (v20) ✅ |
| **UI-6 Open items** ✅ | The leavings of the five waves, each named in the rows above: the `@` file picker (a frontend-side `ignore` walk of `workspace_root()`, re-walked when it is over 30 s old, opening on an `@` token under the caret and browsing into a directory on accept); produced-file chips and the branch action on the turn tail (the chips are derived from the turn's own successful `write-file` / `edit-file` rows, so nothing has to be collected backend-side for them to be true, and branching is `ForkSession`, offered only on the newest finished turn because that is where the fork actually cuts); `/goal edit <text>` with the goal bar's inline objective field; the question takeover's `detail` body and `multi` checkboxes; and the **request envelope** (§10) behind the inspector's Schema / System Prompt / Tools / Options tabs. With it the guide has no Open items left. | **M** | v21 · v22 ✅ |
| **UI-7 Overlay + working directory** ✅ | The last two leavings of UI-3: the `/` and `@` menus move out of the bottom panel into one shared foreground `Area` 4 px above the composer card (pivoted at its bottom edge, so a list that grows or shrinks never nudges the transcript), closing on an outside pointerdown; and the ghost hint after a claimed `/command `, painted at the caret. Alongside them, two things the guide had no row for: the **working directory** (§7.2) — the agent's folder split from the app's own root, picked in Settings › General and passed to the backend child in `SICA_WORKING_DIR` — and the retirement of the Full-access risk gate (§6.7). | **M** | none |
| **UI-8 Workspaces, onboarding, integrations** — **done** | ~~Workspace grouping in the sidebar and Add workspace over `rfd` (§4.3)~~ · ~~the hero picker and the session's workspace as the header crumb (§4.3, §8)~~ · ~~the first-run onboarding modal and "key missing" badges (§7.3)~~ · ~~Settings › Agents and Settings › Integrations (§7.2)~~ · ~~the lightbox and history sizing (§5.3)~~; file cards need harness §9.6 · ~~markdown extras (§3.8)~~ · ~~`@session` (§6.12)~~ · ~~the workflow run body (§6.11)~~ | **L** | v26 (harness Wave 9) |

UI-1 is the visible "looks like dsh" step and is independent of the BE;
UI-2/3 are where the interaction model changes; UI-4/5 are polish and the
power-user view.

---

**UI-8 is done except for one thing that is not UI work.** The §5.3
file cards need generic file attachments to exist (harness §9.6); text files
enter as `@path` references in their place. §6.11's durable events were
built rather than deferred — see below.
One more carry-over sits in §7.1 rather than UI-8: Settings › General still
says "Working directory" and still restarts the backend, because renaming
it to the default-for-ungrouped-sessions *and* dropping the restart needs a
request that sets it on a live backend, which protocol v26 does not have.

## 13. Deliberately not ported

- **Superellipse corners** — egui paints arcs; dsh degrades to arcs on
  engines without `corner-shape`, so this is the supported fallback.
- **Lexical contenteditable with inline chips** — `TextEdit` cannot embed
  widgets; `@path` and `/name` are decorated as coloured runs (galley
  colouring), which is what dsh does *after* send anyway.
- **Workspace drag order and browser-local session orders** — §4.3 ports
  the registry, the grouping and the picker; reorder is Move up / Move down
  and the order is host-durable, one per workspace. The in-app **browse**
  directory dialog is also skipped: FE and BE share a machine, so the OS
  chooser is always available.
- **Brand assets** — whale mark, wordmark, "Into the Unknown", "Preview".
  sica-rust keeps the blade and its name.
- **Locale registry** — worth a `frontend::strings` module of `pub const`s
  (every string above is one), not a runtime registry; one language.
- **No-delete sessions, no unarchive** — product decisions for a hosted
  server; the desktop app keeps Delete behind the armed confirmation.
- **Message feedback, schedule popover** — the features do not exist in the
  harness yet (harness guide §3.7, §12.8); add their surfaces with the
  features. Agent presets and the MCP / hooks / web integrations *do* exist
  since Waves 5–6 — their settings surfaces are §7.2's Agents and
  Integrations rows.
- **Math typesetting** — §3.8 renders TeX source legibly; KaTeX-quality
  layout in egui is its own project.
- **The Cordis inspector** (Chrome DevTools over the host) — the Trajectory
  inspector and `--invariants` are the sica-rust windows into a run.
- **Sidebar collapse choreography** (freeze-fade-slide) and the **turn
  rail** scrubber — nice, later.

---

## 14. egui feasibility notes

- **0.5 px strokes.** egui feathers sub-pixel strokes; on a 1× display a
  0.5 px line renders as a faint 1 px line, which is exactly the intent. On
  2× it is a true half-pixel. Use `Stroke::new(0.5, border)` everywhere
  `HAIRLINE` is called for; do not round to 1.
- **Layered shadows.** `egui::Shadow` is one layer with `offset / blur /
  spread / color` (0.28). `kit::elevated_frame` paints extra layers with
  `Painter::add(Shadow.as_shape(rect, rounding))` before the frame; the
  hairline stroke is a `spread 0.5, blur 0` shadow so it costs no layout,
  matching dsh.
- **Overlays.** Menus, tooltips, toasts, the `/` menu and the modal mask are
  `egui::Area` at `Order::Foreground` / `Order::Tooltip`; close on
  `ctx.input(|i| i.pointer.any_pressed())` outside the rect or `Key::Escape`.
  There is no z-index war because only one overlay kind is open at a time
  (dsh's three disciplines collapse to that rule).
- **Sticky composer + fade.** The composer is a `TopBottomPanel::bottom`
  inside the central panel (as today); the 36 px fade is a vertical
  gradient `Mesh` painted over the transcript's bottom edge.
- **Animation.** `ctx.animate_value_with_time` for width slides and chevron
  rotation; ambient loops (shimmer, sweep, dot chase) use `input.time` and
  `request_repaint_after(16 ms)` *only while something is running*; honour a
  `reduce_motion` setting by freezing them.
- **Container queries** → `ui.available_width()` thresholds (460 for the
  permission chip label, 480 for tail pill labels, 900 for the turn rail).
- **Fonts.** Inter static TTFs (Regular 400, Medium 500, SemiBold 600, Bold
  700) ≈ 1.2 MB total; JetBrains Mono Regular + Bold ≈ 0.5 MB. `FontData::from_static`,
  one family name per weight.
- **Text selection** across the transcript: `drag_to_scroll(false)` stays
  so click-drag selects; egui 0.28 label selection is per-label — the
  Ctrl+A/Ctrl+C message copy remains the reliable path and is kept.
- **Markdown.** `egui_commonmark` 0.17 handles the body; code fences go
  through `kit::code_block` by splitting on fences before rendering (or the
  viewer's syntax-highlighting hook if the `better_syntax_highlighting`
  feature is acceptable — it pulls `syntect`; the shiki-equivalent colour
  table in §1.1 maps onto a syntect theme: keyword `#d6336c`/`#faa2c1`,
  string `#2f9e44`/`#69db7c`, comment `#868e96`/`#adb5bd`, function
  `#6741d9`/`#b197fc`, constant `#1c7ed6`/`#4dabf7`).
