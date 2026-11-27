# The fleet view catalog (mock-up, PRD 2557, round 3)

These are the components the fleet view can draw. The agent that composes the view chooses among them and arranges them however it sees fit; there are no predefined sections, no fixed order, and nothing the desktop adds inside the view. Above the view the deck keeps one line the agent does not control: who composed the view, when, and a warning when that agent has stopped answering.

## Format: A2UI v1.0

The view is a JSON array of A2UI messages:

```json
[
  {"version": "1.0", "createSurface": {"surfaceId": "fleet", "catalogId": "https://agent-deck.devopstoolkit.ai/a2ui/fleet/v1/catalog.json"}},
  {"version": "1.0", "updateComponents": {"surfaceId": "fleet", "components": [ ... ]}},
  {"version": "1.0", "updateDataModel": {"surfaceId": "fleet", "path": "/composedAt", "value": "<ISO 8601 UTC time you composed it>"}}
]
```

`components` is one flat list. Every component is an object with a unique `id` and a `component` type; containers name their children by id; the component with id `root` is drawn first. Any component may carry `weight` (a number): inside a Row it sets that child's share of the width. More `updateDataModel` messages may set the starting value of an input (for example a choice that starts ticked).

`tone`, wherever it appears, is one of `default`, `muted`, `info`, `attention`, `danger`, `ok`. The desktop picks the colours.

## Layout and text

| component | properties | draws |
| --- | --- | --- |
| `Text` | `text`, `variant`: `h1` \| `h2` \| `h3` \| `body` (default) \| `caption` \| `quote`, `tone` | A run of text. Backticked spans are drawn as code; no other markup. |
| `Column` | `children`, `gap`: `s` \| `m` (default) \| `l` | Children top to bottom. |
| `Row` | `children`, `gap`, `align`: `start` \| `center` (default) \| `end`, `wrap`: boolean (default true) | Children side by side. |
| `List` | `children`, `direction`: `vertical` (default) \| `horizontal` | Items with separators between them. |
| `Card` | `children`, `tone` | A bordered panel; a tone colours its edge. `muted` is a borderless, sunken panel. |
| `Divider` | (none) | A horizontal rule. |
| `Icon` | `name`: `bell` \| `alert` \| `check` \| `clock` \| `merge` \| `pause` \| `question` \| `terminal` \| `flask` \| `package` \| `eye` \| `pin` \| `play` \| `x` | A small icon, inline. |

## Pictures

| component | properties | draws |
| --- | --- | --- |
| `Stat` | `value`, `label`, `tone` | A number or short value, large, with a label under it. |
| `Chip` | `text`, `tone` | A small coloured label, such as "blocked" or "needs you". |
| `Progress` | `value`, `max`, `label`, `tone` | A progress bar with its label and "value of max". |
| `Chart` | `kind`: `bar` \| `donut`, `title`, `data`: `[{"label", "value", "tone"}]` | A small bar chart or donut from the numbers given. |
| `Board` | `children` (ids of `Lane`s) | Lanes side by side, like a kanban board. |
| `Lane` | `title`, `tone`, `children` | One column of a Board, with its title and a count of its children. |
| `Timeline` | `items`: `[{"time", "text", "tone"}]` | Events in order, with a time and a dot coloured by tone. |

## Things the deck checks itself

| component | properties | draws |
| --- | --- | --- |
| `DeckRef` | `kind`: `pull_request` \| `issue`, `number`, `detail`: `badge` \| `status` \| `full` (default) | The deck's badge for a pull request or issue. `status` adds the deck's own verdict as a coloured chip: for a pull request "ready to merge" (approved, CI green, no conflict), "needs review", "CI failing", "CI running", "conflicts with main", "merged" or "closed"; for an issue "open" or "closed". `full` adds the review and CI details too. Put pull request and issue state in a DeckRef rather than in your own words: the user trusts the deck's check. |
| `AgentRef` | `agent` (the agent's name as the deck shows it), `detail`: `name` \| `full` (default) | The agent's name and its live status. `full` adds the tool it is running. |

## Asking the user

Every control ends in text sent to an agent. The user sees exactly that text before it goes.

| component | properties | draws |
| --- | --- | --- |
| `ChoicePicker` | `path` (for example `/toClose`), `options` (a list of strings), `multiple`: boolean, `display`: `checkboxes` \| `chips` \| `dropdown`, `label` | A set of choices. The chosen option texts are stored at `path` (a list when `multiple`). |
| `TextField` | `path`, `label`, `placeholder`, `multiline`: boolean | A text box; what the user types is stored at `path`. |
| `PromptButton` | `agent` (name; normally yourself), `text`, `primary`: boolean | A button that sends `text` to that agent. `text` may contain `{/path}` placeholders, filled with what the user chose or typed at that path (a list is joined as "a, b and c"). The button always shows the text it will send, placeholders filled in, and stays disabled while a placeholder it needs is empty. |

## Not here

No colours, fonts or sizes, no HTML, no images, no links other than through DeckRef. A component type outside this catalog is refused before the view reaches the desktop.
