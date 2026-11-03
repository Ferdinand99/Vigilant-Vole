// A small stand-in for the parts of vitest that the script tests use, on top of node:test, so the
// scripts can be tested with `node --test scripts/` and no npm packages (this repository has no
// package.json). Same names as vitest: describe, it, it.each, afterEach, expect.

import assert from "node:assert/strict";
import { afterEach as nodeAfterEach, describe, it as nodeIt } from "node:test";

const format = (name, row) => {
  let i = 0;
  return name.replace(/%[sdif]/g, () => String(row[i++]));
};

export const it = Object.assign(nodeIt, {
  each: (table) => (name, fn) => {
    for (const row of table) nodeIt(format(name, Array.isArray(row) ? row : [row]), () => fn(...(Array.isArray(row) ? row : [row])));
  },
});

export { describe };
export const afterEach = nodeAfterEach;

function matchers(actual, negate) {
  const check = (ok, message) => assert.ok(negate ? !ok : ok, message);
  return {
    toBe: (expected) => (negate ? assert.notStrictEqual(actual, expected) : assert.strictEqual(actual, expected)),
    toEqual: (expected) => (negate ? assert.notDeepStrictEqual(actual, expected) : assert.deepStrictEqual(actual, expected)),
    toContain: (item) => check(actual.includes(item), `expected ${JSON.stringify(actual)} ${negate ? "not " : ""}to contain ${JSON.stringify(item)}`),
    toHaveLength: (n) => check(actual.length === n, `expected length ${n}, got ${actual.length}`),
    toBeTruthy: () => check(Boolean(actual), `expected ${JSON.stringify(actual)} to be truthy`),
    toThrow: (pattern) => {
      let thrown = null;
      try {
        actual();
      } catch (error) {
        thrown = error;
      }
      check(thrown !== null && (pattern === undefined || pattern.test(String(thrown.message))), `expected a throw matching ${pattern}, got ${thrown}`);
    },
  };
}

/** expect(value[, message]).toBe(...) and friends, with `.not`. The message is not used. */
export function expect(actual) {
  return { ...matchers(actual, false), not: matchers(actual, true) };
}
