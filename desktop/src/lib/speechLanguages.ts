/**
 * The languages Settings → Voice → Language offers (issue #1458).
 *
 * The codes live in `speechLanguages.json` beside this file, with the name the
 * select shows for each, because Rust needs the same list: `SpeechLanguage`
 * in `src-tauri/src/settings.rs` folds a code it does not know to English, so
 * a code offered here that Rust lacks is a choice that silently does not
 * stick. `the_speech_languages_match_the_frontends_copy` pins the JSON to
 * `SPEECH_LANGUAGE_TOKENS` code for code.
 *
 * The list is the languages OpenAI documents its speech-to-text as supporting,
 * by ISO-639-1 code, which the local container's Whisper models accept under
 * the same codes.
 */
import SPEECH_LANGUAGES_JSON from "./speechLanguages.json";

/** One language the select offers: the code a request names, and its name. */
export interface SpeechLanguage {
  code: string;
  name: string;
}

export const SPEECH_LANGUAGES: readonly SpeechLanguage[] = SPEECH_LANGUAGES_JSON;

/** The token that names no language: the service detects it per utterance. */
export const AUTO_SPEECH_LANGUAGE = "auto";

/**
 * English, the default — every command phrase the app recognises is English.
 * Mirrors `SpeechLanguage::default()`.
 */
export const DEFAULT_SPEECH_LANGUAGE = "en";

/** Every token the document may store, in `SPEECH_LANGUAGE_TOKENS`' order. */
export const SPEECH_LANGUAGE_TOKENS: readonly string[] = [
  AUTO_SPEECH_LANGUAGE,
  ...SPEECH_LANGUAGES.map((language) => language.code),
];

/**
 * Whether a speech model is an English-only Whisper build, which by naming
 * convention ends in `.en` (`tiny.en`, `Systran/faster-whisper-base.en`, the
 * local default `Systran/faster-whisper-tiny.en`). Such a model transcribes
 * English whatever language the request names, so the panel says so when
 * another language, or Auto, is chosen. A name test, so it can only answer for
 * models that follow the convention.
 */
export const isEnglishOnlySpeechModel = (model: string): boolean => /\.en$/i.test(model.trim());
