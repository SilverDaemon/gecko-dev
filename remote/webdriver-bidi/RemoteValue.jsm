/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

"use strict";

var EXPORTED_SYMBOLS = ["deserialize", "serialize"];

const { XPCOMUtils } = ChromeUtils.import(
  "resource://gre/modules/XPCOMUtils.jsm"
);

const lazy = {};

XPCOMUtils.defineLazyModuleGetters(lazy, {
  assert: "chrome://remote/content/shared/webdriver/Assert.jsm",
  InvalidArgumentError: "chrome://remote/content/shared/webdriver/Errors.jsm",
  Log: "chrome://remote/content/shared/Log.jsm",
});

XPCOMUtils.defineLazyGetter(lazy, "logger", () =>
  lazy.Log.get(lazy.Log.TYPES.WEBDRIVER_BIDI)
);

/**
 * Deserialize a local value.
 *
 * @see https://w3c.github.io/webdriver-bidi/#deserialize-local-value
 *
 * @param {Object} serializedValue
 *     Value of any type to be deserialized.
 *
 * @returns {Object} Deserialized representation of the value.
 */
function deserialize(serializedValue) {
  const { objectId, type, value } = serializedValue;

  if (type !== undefined) {
    lazy.assert.string(type, `Expected "type" to be a string, got ${type}`);
  }

  // With an objectId present deserialize as remote reference.
  if (objectId !== undefined) {
    lazy.assert.string(
      objectId,
      `Expected "objectId" to be a string, got ${objectId}`
    );

    // TODO: Implement deserialization of remote references (bug 1693838)
    lazy.logger.warn(
      `Unsupported type remote reference with objectId ${objectId}`
    );
    return undefined;
  }

  // Primitive protocol values
  switch (type) {
    case "undefined":
      return undefined;
    case "null":
      return null;
    case "string":
      lazy.assert.string(
        value,
        `Expected "value" to be a string, got ${value}`
      );
      return value;
    case "number":
      // If value is already a number return its value.
      if (typeof value === "number") {
        return value;
      }

      // Otherwise it has to be one of the special strings
      lazy.assert.in(value, ["NaN", "-0", "+Infinity", "-Infinity"]);
      return Number(value);
    case "boolean":
      lazy.assert.boolean(
        value,
        `Expected "value" to be a boolean, got ${value}`
      );
      return value;
    case "bigint":
      lazy.assert.string(
        value,
        `Expected "value" to be a string, got ${value}`
      );
      try {
        return BigInt(value);
      } catch (e) {
        throw new lazy.InvalidArgumentError(
          `Failed to deserialize value as BigInt: ${value}`
        );
      }
  }

  lazy.logger.warn(`Unsupported type for local value ${type}`);
  return undefined;
}

/**
 * Serialize a value as a remote value.
 *
 * @see https://w3c.github.io/webdriver-bidi/#serialize-as-a-remote-value
 *
 * @param {Object} value
 *     Value of any type to be serialized.
 *
 * @returns {Object} Serialized representation of the value.
 */
function serialize(value /*, maxDepth, nodeDetails, knownObjects */) {
  const type = typeof value;

  // Primitive protocol values
  if (type == "undefined") {
    return { type };
  } else if (Object.is(value, null)) {
    return { type: "null" };
  } else if (Object.is(value, NaN)) {
    return { type: "number", value: "NaN" };
  } else if (Object.is(value, -0)) {
    return { type: "number", value: "-0" };
  } else if (Object.is(value, Infinity)) {
    return { type: "number", value: "+Infinity" };
  } else if (Object.is(value, -Infinity)) {
    return { type: "number", value: "-Infinity" };
  } else if (type == "bigint") {
    return { type, value: value.toString() };
  } else if (["boolean", "number", "string"].includes(type)) {
    return { type, value };
  }

  lazy.logger.warn(`Unsupported type for remote value: ${value.toString()}`);
  return undefined;
}
