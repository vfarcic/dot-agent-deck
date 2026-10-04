# Speech fixtures

Real (synthesised) speech for the voice check in `voice/human_voice.rs` and its tests in `voice/transcribe.rs` (issue #1450). Each file is raw 16 kHz mono signed 16-bit little-endian PCM with no header, trimmed to the speech plus 40 ms either side; the tests add room tone around it.

They were synthesised on 2026-10-04 with [piper](https://github.com/rhasspy/piper) voices served by the local speech container this app's local backend already uses (`ghcr.io/speaches-ai/speaches:0.9.0-rc.3-cpu`), through its `/v1/audio/speech` endpoint with `"response_format": "pcm"` and `"sample_rate": 16000`:

| file | voice | speed | voice's training data licence |
| --- | --- | --- | --- |
| `*-john.pcm` | `speaches-ai/piper-en_US-john-medium` | 1.0 | public domain (LibriVox) |
| `*-kristin.pcm` | `speaches-ai/piper-en_US-kristin-medium` | 1.5 | public domain (LibriVox) |
| `dictation-joe.pcm` | `speaches-ai/piper-en_US-joe-medium` | 1.0 | CC0 |

These three were chosen because their model cards list public-domain or CC0 training data; voices trained on non-commercial data (`ryan`, `hfc_male`, among others) were avoided.

To regenerate one, with the container running on port 18000 and the voice downloaded (`POST /v1/models/<model>`):

```sh
curl -s http://127.0.0.1:18000/v1/audio/speech -H 'content-type: application/json' \
  -d '{"model":"speaches-ai/piper-en_US-john-medium","voice":"john","input":"Send it.","response_format":"pcm","sample_rate":16000,"speed":1.0}' \
  -o send-it-john.pcm
```

then trim the leading and trailing silence. A regenerated file is not byte-identical to the committed one, and the tests do not need it to be.
