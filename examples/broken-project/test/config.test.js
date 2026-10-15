const test = require('node:test');
const assert = require('node:assert');
const { loadConfig } = require('../src/config.js');

test('loadConfig 读出 appName 与 port', () => {
  const config = loadConfig();
  assert.strictEqual(config.appName, 'broken-project');
  assert.strictEqual(config.port, 8080);
});
