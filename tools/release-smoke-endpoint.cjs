const fs = require('node:fs');

function readEndpoint(file, readFile = fs.readFileSync) {
  let contents;
  try {
    contents = readFile(file, 'utf8');
  } catch (error) {
    // Creation notifications can precede the writer releasing its Windows lock.
    // Keep waiting for the next filesystem change; never poll or mask other errors.
    if (error.code === 'ENOENT' || error.code === 'EBUSY') return null;
    throw error;
  }
  const [value, browserPath] = contents.split(/\r?\n/);
  if (!/^\d+$/.test(value) || !browserPath?.startsWith('/devtools/browser/')) return null;
  return { port: Number(value), browserPath };
}

module.exports = { readEndpoint };
