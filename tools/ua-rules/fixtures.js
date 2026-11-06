// Writes `crate-ua-rules/tests/fixtures/ua.json`: for a corpus of real user agents,
// what the JS libraries return. The Rust port is tested against these.
//
//   npm ci --ignore-scripts && npm run fixtures
//
// `device` is ua-parser-js 1.x (the ported version); `device_upstream` is 2.x
// (what upstream Umami runs), recorded to track how far the two drift.

const fs = require('node:fs');
const path = require('node:path');
const { isbot } = require('isbot');
const { browserName, detectOS } = require('detect-browser');
const { UAParser } = require('ua-parser-js');
const UAParserV1 = require('ua-parser-js-v1');

const OUT = path.resolve(__dirname, '../../crate-ua-rules/tests/fixtures/ua.json');

// User-agent corpora: isbot's (Unlicense) and ua-parser-js's (MIT) test suites.
const CORPORA = [
  ['yaml', 'https://raw.githubusercontent.com/omrilotan/isbot/v5.2.2/fixtures/browsers.yml'],
  ['yaml', 'https://raw.githubusercontent.com/omrilotan/isbot/v5.2.2/fixtures/crawlers.yml'],
  ['json', 'https://raw.githubusercontent.com/faisalman/ua-parser-js/1.0.41/test/device-test.json'],
  ['json', 'https://raw.githubusercontent.com/faisalman/ua-parser-js/1.0.41/test/os-test.json'],
  ['json', 'https://raw.githubusercontent.com/faisalman/ua-parser-js/1.0.41/test/browser-test.json'],
];

function parseYaml(text) {
  return text
    .split('\n')
    .filter(line => line.startsWith('  - '))
    .map(line => line.slice(4).trim())
    .map(value => {
      if (value.startsWith("'") && value.endsWith("'")) return value.slice(1, -1).replace(/''/g, "'");
      if (value.startsWith('"') && value.endsWith('"')) return JSON.parse(value);
      return value;
    });
}

async function main() {
  const uas = new Set();

  for (const [format, url] of CORPORA) {
    const response = await fetch(url);
    if (!response.ok) throw new Error(`${url}: ${response.status}`);
    const text = await response.text();
    const entries = format === 'yaml' ? parseYaml(text) : JSON.parse(text).map(test => test.ua);

    for (const ua of entries) {
      if (typeof ua === 'string' && ua && !ua.includes('\n')) uas.add(ua);
    }
  }

  const rows = [...uas].map(ua => ({
    ua,
    bot: isbot(ua),
    browser: browserName(ua),
    os: detectOS(ua),
    device: new UAParserV1(ua).getDevice().type ?? null,
    device_upstream: UAParser(ua).device.type ?? null,
  }));

  fs.mkdirSync(path.dirname(OUT), { recursive: true });
  fs.writeFileSync(OUT, `[\n${rows.map(row => JSON.stringify(row)).join(',\n')}\n]\n`);

  const drift = rows.filter(row => row.device !== row.device_upstream).length;
  console.log(`user agents=${rows.length} bots=${rows.filter(r => r.bot).length} device drift=${drift}`);
}

main().catch(error => {
  console.error(error);
  process.exit(1);
});
