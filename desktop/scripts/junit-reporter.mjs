// The driver tier's JUnit reporter (`pnpm test:driver`, issue #1981 M10).
// Node's built-in junit reporter writes top-level `test()` cases straight under
// `<testsuites>`, and RWX's generic JUnit parser ignores a case with no
// `<testsuite>` around it, so it ingested none of them. This wraps the built-in
// report in one `<testsuite name="node:test">`; pass/fail and the exit code are
// node:test's own.
//
// Limit: verified for the driver tier's current top-level `test()` cases. If
// `describe()` suites are added, the built-in reporter already nests those in
// `<testsuite>` elements, and this wrapping must be revisited.
import { junit } from 'node:test/reporters';

export default async function* junitWithSuite(source) {
  let xml = '';
  for await (const chunk of junit(source)) xml += chunk;
  if (!xml.includes('<testsuites>') || !xml.includes('</testsuites>')) {
    throw new Error('node:test JUnit report has an unexpected root');
  }
  yield xml.replace('<testsuites>', '<testsuites><testsuite name="node:test">')
    .replace('</testsuites>', '</testsuite></testsuites>');
}
