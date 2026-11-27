import { test } from "node:test";
import assert from "node:assert/strict";
import { fit } from "../src/lib/photo.js";

test("a photo shrinks to fit its long side and never grows", () => {
  assert.equal(fit(4032, 3024), 1600 / 4032);
  assert.equal(fit(3024, 4032), 1600 / 4032);
  assert.equal(fit(800, 600), 1);
});
