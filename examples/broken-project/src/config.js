const fs = require('node:fs');
const path = require('node:path');
function loadConfig() {
  const configPath = path.join(__dirname, '..', '..', 'config', 'app.json');
  return JSON.parse(fs.readFileSync(configPath, 'utf8'));
}
module.exports = { loadConfig };
