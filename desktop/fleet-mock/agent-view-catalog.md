# The fleet catalog (mock-up, PRD 2557)

The agent composes the whole fleet view from these components and nothing else. The desktop draws them with its own components and styles. The deck keeps one thin strip above the view, which the agent does not control: who composed it, when, and a warning when the agent has stopped answering. Everything below that strip is the agent's.

## Format: A2UI v1.0

Output is a JSON array of three messages:

```json
[
  {"version": "1.0", "createSurface": {"surfaceId": "fleet", "catalogId": "https://agent-deck.devopstoolkit.ai/a2ui/fleet/v1/catalog.json"}},
  {"version": "1.0", "updateComponents": {"surfaceId": "fleet", "components": [ ... ]}},
  {"version": "1.0", "updateDataModel": {"surfaceId": "fleet", "path": "/composedAt", "value": "2026-11-27T13:17:00Z"}}
]
```

`components` is one flat list. Every component is an object with a unique `id` and a `component` type. Containers name their children by id. The component with id `root` is drawn first. Any component may carry `weight` (a number): inside a Row it sets the share of the width that child takes.

## Layout and text

| component | properties | draws |
| --- | --- | --- |
| `Text` | `text`, `variant`: `h1` \| `h2` \| `h3` \| `body` (default) \| `caption` \| `quote`, `tone`: `default` \| `muted` \| `attention` \| `danger` \| `ok` | A run of text. Backticked spans are drawn as code. No other markup. |
| `Column` | `children`, `gap`: `s` \| `m` (default) \| `l` | Children top to bottom. |
| `Row` | `children`, `gap`, `align`: `start` \| `center` (default) \| `end`, `wrap`: boolean (default true) | Children side by side. Use `weight` on children for columns of different widths. |
| `List` | `children`, `direction`: `vertical` (default) \| `horizontal` | A list with separators between items. |
| `Card` | `children`, `tone`: `default` \| `attention` \| `danger` \| `ok` \| `quiet` | A bordered panel. `attention` draws an amber edge, `danger` a red one, `ok` a green one, `quiet` no border and a sunken background. |
| `Divider` | (none) | A horizontal rule. |
| `Icon` | `name`: `bell` \| `alert` \| `check` \| `clock` \| `merge` \| `pause` \| `question` \| `terminal` \| `flask` \| `package` \| `eye` \| `pin` | A small icon, inline. |
| `Stat` | `value`, `label`, `tone` | A number or short value, large, with a label under it. |

## Deck components: the deck fills in what it has checked

| component | properties | draws |
| --- | --- | --- |
| `DeckRef` | `kind`: `pull_request` \| `issue`, `number`, `detail`: `badge` \| `full` (default) | The deck's badge for that pull request or issue. With `full`, also the state the deck checked itself: open/merged/closed, review, CI, and "conflicts with main" when it does. The agent never writes those states in its own words next to a DeckRef; the deck's check is what the user trusts. |
| `AgentRef` | `agent` (the agent's name), `detail`: `name` \| `full` (default) | The agent's name and its live status pill. With `full`, also its pane's tool and wake. |
| `PromptButton` | `agent` (name), `text`, `primary`: boolean | A button. Pressing it sends `text` to that agent, exactly as shown. A button can only send what it shows. |
| `ReplyButton` | `agent` (name), `label` (optional, default "Reply") | Opens a text box; whatever the user types goes to that agent. |
| `DeckAction` | `operation`: `close` \| `open-pane`, `agents` (list of names), `label` | A deck operation. `close` asks for confirmation with the deck's own dialog before closing; `open-pane` opens the agent's terminal. |

## What is not here

No colours, no fonts, no sizes, no HTML, no images, no links other than through DeckRef. A component type outside this catalog is refused by the daemon before the view reaches the desktop.
