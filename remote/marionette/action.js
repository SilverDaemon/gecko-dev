/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this file,
 * You can obtain one at http://mozilla.org/MPL/2.0/. */

/* eslint no-dupe-keys:off */
/* eslint-disable no-restricted-globals */

"use strict";

const EXPORTED_SYMBOLS = ["action"];

const { XPCOMUtils } = ChromeUtils.import(
  "resource://gre/modules/XPCOMUtils.jsm"
);

const lazy = {};

XPCOMUtils.defineLazyModuleGetters(lazy, {
  AppInfo: "chrome://remote/content/marionette/appinfo.js",
  assert: "chrome://remote/content/shared/webdriver/Assert.jsm",
  element: "chrome://remote/content/marionette/element.js",
  error: "chrome://remote/content/shared/webdriver/Errors.jsm",
  event: "chrome://remote/content/marionette/event.js",
  pprint: "chrome://remote/content/shared/Format.jsm",
  Sleep: "chrome://remote/content/marionette/sync.js",
});

// TODO? With ES 2016 and Symbol you can make a safer approximation
// to an enum e.g. https://gist.github.com/xmlking/e86e4f15ec32b12c4689
/**
 * Implements WebDriver Actions API: a low-level interface for providing
 * virtualised device input to the web browser.
 *
 * @namespace
 */
const action = {
  Pause: "pause",
  KeyDown: "keyDown",
  KeyUp: "keyUp",
  PointerDown: "pointerDown",
  PointerUp: "pointerUp",
  PointerMove: "pointerMove",
  PointerCancel: "pointerCancel",
};

const ACTIONS = {
  none: new Set([action.Pause]),
  key: new Set([action.Pause, action.KeyDown, action.KeyUp]),
  pointer: new Set([
    action.Pause,
    action.PointerDown,
    action.PointerUp,
    action.PointerMove,
    action.PointerCancel,
  ]),
};

/** Map from normalized key value to UI Events modifier key name */
const MODIFIER_NAME_LOOKUP = {
  Alt: "alt",
  Shift: "shift",
  Control: "ctrl",
  Meta: "meta",
};

/** Represents possible values for a pointer-move origin. */
action.PointerOrigin = {
  Viewport: "viewport",
  Pointer: "pointer",
};

/** Flag for WebDriver spec conforming pointer origin calculation. */
action.specCompatPointerOrigin = true;

/**
 * Look up a PointerOrigin.
 *
 * @param {(string|Element)=} obj
 *     Origin for a <code>pointerMove</code> action.  Must be one of
 *     "viewport" (default), "pointer", or a DOM element.
 *
 * @return {action.PointerOrigin}
 *     Pointer origin.
 *
 * @throws {InvalidArgumentError}
 *     If <var>obj</var> is not a valid origin.
 */
action.PointerOrigin.get = function(obj) {
  let origin = obj;
  if (typeof obj == "undefined") {
    origin = this.Viewport;
  } else if (typeof obj == "string") {
    let name = capitalize(obj);
    lazy.assert.in(
      name,
      this,
      lazy.pprint`Unknown pointer-move origin: ${obj}`
    );
    origin = this[name];
  } else if (!lazy.element.isElement(obj)) {
    throw new lazy.error.InvalidArgumentError(
      "Expected 'origin' to be undefined, " +
        '"viewport", "pointer", ' +
        lazy.pprint`or an element, got: ${obj}`
    );
  }
  return origin;
};

/** Represents possible subtypes for a pointer input source. */
action.PointerType = {
  Mouse: "mouse",
  // TODO For now, only mouse is supported
  // Pen: "pen",
  // Touch: "touch",
};

/**
 * Look up a PointerType.
 *
 * @param {string} str
 *     Name of pointer type.
 *
 * @return {string}
 *     A pointer type for processing pointer parameters.
 *
 * @throws {InvalidArgumentError}
 *     If <code>str</code> is not a valid pointer type.
 */
action.PointerType.get = function(str) {
  let name = capitalize(str);
  lazy.assert.in(name, this, lazy.pprint`Unknown pointerType: ${str}`);
  return this[name];
};

/**
 * Input state associated with current session.  This is a map between
 * input ID and the device state for that input source, with one entry
 * for each active input source.
 */
action.inputStateMap = new Map();

/**
 * List of {@link action.Action} associated with current session.  Used to
 * manage dispatching events when resetting the state of the input sources.
 * Reset operations are assumed to be idempotent.
 */
action.inputsToCancel = [];

/**
 * Represents device state for an input source.
 */
class InputState {
  constructor() {
    this.type = this.constructor.name.toLowerCase();
  }

  /**
   * Check equality of this InputState object with another.
   *
   * @param {InputState} other
   *     Object representing an input state.
   *
   * @return {boolean}
   *     True if <code>this</code> has the same <code>type</code>
   *     as <code>other</code>.
   */
  is(other) {
    if (typeof other == "undefined") {
      return false;
    }
    return this.type === other.type;
  }

  toString() {
    return `[object ${this.constructor.name}InputState]`;
  }

  /**
   * @param {Object.<string, ?>} obj
   *     Object with property <code>type</code> and optionally
   *     <code>parameters</code> or <code>pointerType</code>,
   *     representing an action sequence or an action item.
   *
   * @return {action.InputState}
   *     An {@link InputState} object for the type of the
   *     {@link actionSequence}.
   *
   * @throws {InvalidArgumentError}
   *     If {@link actionSequence.type} is not valid.
   */
  static fromJSON(obj) {
    let type = obj.type;
    lazy.assert.in(type, ACTIONS, lazy.pprint`Unknown action type: ${type}`);
    let name = type == "none" ? "Null" : capitalize(type);
    if (name == "Pointer") {
      if (
        !obj.pointerType &&
        (!obj.parameters || !obj.parameters.pointerType)
      ) {
        throw new lazy.error.InvalidArgumentError(
          lazy.pprint`Expected obj to have pointerType, got ${obj}`
        );
      }
      let pointerType = obj.pointerType || obj.parameters.pointerType;
      return new action.InputState[name](pointerType);
    }
    return new action.InputState[name]();
  }
}

/** Possible kinds of |InputState| for supported input sources. */
action.InputState = {};

/**
 * Input state associated with a keyboard-type device.
 */
action.InputState.Key = class Key extends InputState {
  constructor() {
    super();
    this.pressed = new Set();
    this.alt = false;
    this.shift = false;
    this.ctrl = false;
    this.meta = false;
  }

  /**
   * Update modifier state according to |key|.
   *
   * @param {string} key
   *     Normalized key value of a modifier key.
   * @param {boolean} value
   *     Value to set the modifier attribute to.
   *
   * @throws {InvalidArgumentError}
   *     If |key| is not a modifier.
   */
  setModState(key, value) {
    if (key in MODIFIER_NAME_LOOKUP) {
      this[MODIFIER_NAME_LOOKUP[key]] = value;
    } else {
      throw new lazy.error.InvalidArgumentError(
        "Expected 'key' to be one of " +
          Object.keys(MODIFIER_NAME_LOOKUP) +
          lazy.pprint`, got ${key}`
      );
    }
  }

  /**
   * Check whether |key| is pressed.
   *
   * @param {string} key
   *     Normalized key value.
   *
   * @return {boolean}
   *     True if |key| is in set of pressed keys.
   */
  isPressed(key) {
    return this.pressed.has(key);
  }

  /**
   * Add |key| to the set of pressed keys.
   *
   * @param {string} key
   *     Normalized key value.
   *
   * @return {boolean}
   *     True if |key| is in list of pressed keys.
   */
  press(key) {
    return this.pressed.add(key);
  }

  /**
   * Remove |key| from the set of pressed keys.
   *
   * @param {string} key
   *     Normalized key value.
   *
   * @return {boolean}
   *     True if |key| was present before removal, false otherwise.
   */
  release(key) {
    return this.pressed.delete(key);
  }
};

/**
 * Input state not associated with a specific physical device.
 */
action.InputState.Null = class Null extends InputState {
  constructor() {
    super();
    this.type = "none";
  }
};

/**
 * Input state associated with a pointer-type input device.
 *
 * @param {string} subtype
 *     Kind of pointing device: mouse, pen, touch.
 *
 * @throws {InvalidArgumentError}
 *     If subtype is undefined or an invalid pointer type.
 */
action.InputState.Pointer = class Pointer extends InputState {
  constructor(subtype) {
    super();
    this.pressed = new Set();
    lazy.assert.defined(
      subtype,
      lazy.pprint`Expected subtype to be defined, got ${subtype}`
    );
    this.subtype = action.PointerType.get(subtype);
    this.x = 0;
    this.y = 0;
  }

  /**
   * Check whether |button| is pressed.
   *
   * @param {number} button
   *     Positive integer that refers to a mouse button.
   *
   * @return {boolean}
   *     True if |button| is in set of pressed buttons.
   */
  isPressed(button) {
    lazy.assert.positiveInteger(button);
    return this.pressed.has(button);
  }

  /**
   * Add |button| to the set of pressed keys.
   *
   * @param {number} button
   *     Positive integer that refers to a mouse button.
   *
   * @return {Set}
   *     Set of pressed buttons.
   */
  press(button) {
    lazy.assert.positiveInteger(button);
    return this.pressed.add(button);
  }

  /**
   * Remove |button| from the set of pressed buttons.
   *
   * @param {number} button
   *     A positive integer that refers to a mouse button.
   *
   * @return {boolean}
   *     True if |button| was present before removals, false otherwise.
   */
  release(button) {
    lazy.assert.positiveInteger(button);
    return this.pressed.delete(button);
  }
};

/**
 * Repesents an action for dispatch. Used in |action.Chain| and
 * |action.Sequence|.
 *
 * @param {string} id
 *     Input source ID.
 * @param {string} type
 *     Action type: none, key, pointer.
 * @param {string} subtype
 *     Action subtype: {@link action.Pause}, {@link action.KeyUp},
 *     {@link action.KeyDown}, {@link action.PointerUp},
 *     {@link action.PointerDown}, {@link action.PointerMove}, or
 *     {@link action.PointerCancel}.
 *
 * @throws {InvalidArgumentError}
 *      If any parameters are undefined.
 */
action.Action = class {
  constructor(id, type, subtype) {
    if ([id, type, subtype].includes(undefined)) {
      throw new lazy.error.InvalidArgumentError("Missing id, type or subtype");
    }
    for (let attr of [id, type, subtype]) {
      lazy.assert.string(attr, lazy.pprint`Expected string, got ${attr}`);
    }
    this.id = id;
    this.type = type;
    this.subtype = subtype;
  }

  toString() {
    return `[action ${this.type}]`;
  }

  /**
   * @param {action.Sequence} actionSequence
   *     Object representing sequence of actions from one input source.
   * @param {action.Action} actionItem
   *     Object representing a single action from |actionSequence|.
   *
   * @return {action.Action}
   *     An action that can be dispatched; corresponds to |actionItem|.
   *
   * @throws {InvalidArgumentError}
   *     If any <code>actionSequence</code> or <code>actionItem</code>
   *     attributes are invalid.
   * @throws {UnsupportedOperationError}
   *     If <code>actionItem.type</code> is {@link action.PointerCancel}.
   */
  static fromJSON(actionSequence, actionItem) {
    let type = actionSequence.type;
    let id = actionSequence.id;
    let subtypes = ACTIONS[type];
    if (!subtypes) {
      throw new lazy.error.InvalidArgumentError("Unknown type: " + type);
    }
    let subtype = actionItem.type;
    if (!subtypes.has(subtype)) {
      throw new lazy.error.InvalidArgumentError(
        `Unknown subtype for ${type} action: ${subtype}`
      );
    }

    let item = new action.Action(id, type, subtype);
    if (type === "pointer") {
      action.processPointerAction(
        id,
        action.PointerParameters.fromJSON(actionSequence.parameters),
        item
      );
    }

    switch (item.subtype) {
      case action.KeyUp:
      case action.KeyDown:
        let key = actionItem.value;
        // TODO countGraphemes
        // TODO key.value could be a single code point like "\uE012"
        // (see rawKey) or "grapheme cluster"
        lazy.assert.string(
          key,
          "Expected 'value' to be a string that represents single code point " +
            lazy.pprint`or grapheme cluster, got ${key}`
        );
        item.value = key;
        break;

      case action.PointerDown:
      case action.PointerUp:
        lazy.assert.positiveInteger(
          actionItem.button,
          lazy.pprint`Expected 'button' (${actionItem.button}) to be >= 0`
        );
        item.button = actionItem.button;
        break;

      case action.PointerMove:
        item.duration = actionItem.duration;
        if (typeof item.duration != "undefined") {
          lazy.assert.positiveInteger(
            item.duration,
            lazy.pprint`Expected 'duration' (${item.duration}) to be >= 0`
          );
        }
        item.origin = action.PointerOrigin.get(actionItem.origin);
        item.x = actionItem.x;
        if (typeof item.x != "undefined") {
          lazy.assert.integer(
            item.x,
            lazy.pprint`Expected 'x' (${item.x}) to be an Integer`
          );
        }
        item.y = actionItem.y;
        if (typeof item.y != "undefined") {
          lazy.assert.integer(
            item.y,
            lazy.pprint`Expected 'y' (${item.y}) to be an Integer`
          );
        }
        break;

      case action.PointerCancel:
        throw new lazy.error.UnsupportedOperationError();

      case action.Pause:
        item.duration = actionItem.duration;
        if (typeof item.duration != "undefined") {
          // eslint-disable-next-line
          lazy.assert.positiveInteger(item.duration,
            lazy.pprint`Expected 'duration' (${item.duration}) to be >= 0`
          );
        }
        break;
    }

    return item;
  }
};

/**
 * Represents a series of ticks, specifying which actions to perform at
 * each tick.
 */
action.Chain = class extends Array {
  toString() {
    return `[chain ${super.toString()}]`;
  }

  /**
   * @param {Array.<?>} actions
   *     Array of objects that each represent an action sequence.
   *
   * @return {action.Chain}
   *     Transpose of <var>actions</var> such that actions to be performed
   *     in a single tick are grouped together.
   *
   * @throws {InvalidArgumentError}
   *     If <var>actions</var> is not an Array.
   */
  static fromJSON(actions) {
    lazy.assert.array(
      actions,
      lazy.pprint`Expected 'actions' to be an array, got ${actions}`
    );

    let actionsByTick = new action.Chain();
    for (let actionSequence of actions) {
      // TODO(maja_zf): Check that each actionSequence in actions refers
      // to a different input ID.
      let inputSourceActions = action.Sequence.fromJSON(actionSequence);
      for (let i = 0; i < inputSourceActions.length; i++) {
        // new tick
        if (actionsByTick.length < i + 1) {
          actionsByTick.push([]);
        }
        actionsByTick[i].push(inputSourceActions[i]);
      }
    }
    return actionsByTick;
  }
};

/**
 * Represents one input source action sequence; this is essentially an
 * |Array.<action.Action>|.
 */
action.Sequence = class extends Array {
  toString() {
    return `[sequence ${super.toString()}]`;
  }

  /**
   * @param {Object.<string, ?>} actionSequence
   *     Object that represents a sequence action items for one input source.
   *
   * @return {action.Sequence}
   *     Sequence of actions that can be dispatched.
   *
   * @throws {InvalidArgumentError}
   *     If <code>actionSequence.id</code> is not a
   *     string or it's aleady mapped to an |action.InputState}
   *     incompatible with <code>actionSequence.type</code>, or if
   *     <code>actionSequence.actions</code> is not an <code>Array</code>.
   */
  static fromJSON(actionSequence) {
    // used here to validate 'type' in addition to InputState type below
    let inputSourceState = InputState.fromJSON(actionSequence);
    let id = actionSequence.id;
    lazy.assert.defined(id, "Expected 'id' to be defined");
    lazy.assert.string(
      id,
      lazy.pprint`Expected 'id' to be a string, got ${id}`
    );
    let actionItems = actionSequence.actions;
    lazy.assert.array(
      actionItems,
      "Expected 'actionSequence.actions' to be an array, " +
        lazy.pprint`got ${actionSequence.actions}`
    );

    if (!action.inputStateMap.has(id)) {
      action.inputStateMap.set(id, inputSourceState);
    } else if (!action.inputStateMap.get(id).is(inputSourceState)) {
      throw new lazy.error.InvalidArgumentError(
        `Expected ${id} to be mapped to ${inputSourceState}, ` +
          `got ${action.inputStateMap.get(id)}`
      );
    }

    let actions = new action.Sequence();
    for (let actionItem of actionItems) {
      actions.push(action.Action.fromJSON(actionSequence, actionItem));
    }

    return actions;
  }
};

/**
 * Represents parameters in an action for a pointer input source.
 *
 * @param {string=} pointerType
 *     Type of pointing device.  If the parameter is undefined, "mouse"
 *     is used.
 */
action.PointerParameters = class {
  constructor(pointerType = "mouse") {
    this.pointerType = action.PointerType.get(pointerType);
  }

  toString() {
    return `[pointerParameters ${this.pointerType}]`;
  }

  /**
   * @param {Object.<string, ?>} parametersData
   *     Object that represents pointer parameters.
   *
   * @return {action.PointerParameters}
   *     Validated pointer paramters.
   */
  static fromJSON(parametersData) {
    if (typeof parametersData == "undefined") {
      return new action.PointerParameters();
    }
    return new action.PointerParameters(parametersData.pointerType);
  }
};

/**
 * Adds <var>pointerType</var> attribute to Action <var>act</var>.
 *
 * Helper function for {@link action.Action.fromJSON}.
 *
 * @param {string} id
 *     Input source ID.
 * @param {action.PointerParams} pointerParams
 *     Input source pointer parameters.
 * @param {action.Action} act
 *     Action to be updated.
 *
 * @throws {InvalidArgumentError}
 *     If <var>id</var> is already mapped to an
 *     {@link action.InputState} that is not compatible with
 *     <code>act.type</code> or <code>pointerParams.pointerType</code>.
 */
action.processPointerAction = function(id, pointerParams, act) {
  if (
    action.inputStateMap.has(id) &&
    action.inputStateMap.get(id).type !== act.type
  ) {
    throw new lazy.error.InvalidArgumentError(
      `Expected 'id' ${id} to be mapped to InputState whose type is ` +
        action.inputStateMap.get(id).type +
        lazy.pprint` , got ${act.type}`
    );
  }
  let pointerType = pointerParams.pointerType;
  if (
    action.inputStateMap.has(id) &&
    action.inputStateMap.get(id).subtype !== pointerType
  ) {
    throw new lazy.error.InvalidArgumentError(
      `Expected 'id' ${id} to be mapped to InputState whose subtype is ` +
        action.inputStateMap.get(id).subtype +
        lazy.pprint` , got ${pointerType}`
    );
  }
  act.pointerType = pointerParams.pointerType;
};

/** Collect properties associated with KeyboardEvent */
action.Key = class {
  constructor(rawKey) {
    const { key, code, location, printable } = lazy.event.getKeyData(rawKey);
    this.key = key;
    this.code = code;
    this.location = location;
    this.printable = printable;
    this.altKey = false;
    this.shiftKey = false;
    this.ctrlKey = false;
    this.metaKey = false;
    this.repeat = false;
    // keyCode will be computed by event.sendKeyDown
  }

  update(inputState) {
    this.altKey = inputState.alt;
    this.shiftKey = inputState.shift;
    this.ctrlKey = inputState.ctrl;
    this.metaKey = inputState.meta;
  }
};

/** Collect properties associated with MouseEvent */
action.Mouse = class {
  constructor(type, button = 0) {
    this.type = type;
    lazy.assert.positiveInteger(button);
    this.button = button;
    this.buttons = 0;
    this.altKey = false;
    this.shiftKey = false;
    this.metaKey = false;
    this.ctrlKey = false;
    // set modifier properties based on whether any corresponding keys are
    // pressed on any key input source
    for (let inputState of action.inputStateMap.values()) {
      if (inputState.type == "key") {
        this.altKey = inputState.alt || this.altKey;
        this.ctrlKey = inputState.ctrl || this.ctrlKey;
        this.metaKey = inputState.meta || this.metaKey;
        this.shiftKey = inputState.shift || this.shiftKey;
      }
    }
  }

  update(inputState) {
    let allButtons = Array.from(inputState.pressed);
    this.buttons = allButtons.reduce((a, i) => a + Math.pow(2, i), 0);
  }
};

/**
 * Dispatch a chain of actions over |chain.length| ticks.
 *
 * This is done by creating a Promise for each tick that resolves once
 * all the Promises for individual tick-actions are resolved.  The next
 * tick's actions are not dispatched until the Promise for the current
 * tick is resolved.
 *
 * @param {action.Chain} chain
 *     Actions grouped by tick; each element in |chain| is a sequence of
 *     actions for one tick.
 * @param {WindowProxy} win
 *     Current window global.
 * @param {boolean=} [specCompatPointerOrigin=true] specCompatPointerOrigin
 *     Flag to turn off the WebDriver spec conforming pointer origin
 *     calculation. It has to be kept until all Selenium bindings can
 *     successfully handle the WebDriver spec conforming Pointer Origin
 *     calculation. See https://bugzilla.mozilla.org/show_bug.cgi?id=1429338.
 *
 * @return {Promise}
 *     Promise for dispatching all actions in |chain|.
 */
action.dispatch = function(chain, win, specCompatPointerOrigin = true) {
  action.specCompatPointerOrigin = specCompatPointerOrigin;

  let chainEvents = (async () => {
    for (let tickActions of chain) {
      await action.dispatchTickActions(
        tickActions,
        action.computeTickDuration(tickActions),
        win
      );
    }
  })();
  return chainEvents;
};

/**
 * Dispatch sequence of actions for one tick.
 *
 * This creates a Promise for one tick that resolves once the Promise
 * for each tick-action is resolved, which takes at least |tickDuration|
 * milliseconds.  The resolved set of events for each tick is followed by
 * firing of pending DOM events.
 *
 * Note that the tick-actions are dispatched in order, but they may have
 * different durations and therefore may not end in the same order.
 *
 * @param {Array.<action.Action>} tickActions
 *     List of actions for one tick.
 * @param {number} tickDuration
 *     Duration in milliseconds of this tick.
 * @param {WindowProxy} win
 *     Current window global.
 *
 * @return {Promise}
 *     Promise for dispatching all tick-actions and pending DOM events.
 */
action.dispatchTickActions = function(tickActions, tickDuration, win) {
  let pendingEvents = tickActions.map(toEvents(tickDuration, win));
  return Promise.all(pendingEvents);
};

/**
 * Compute tick duration in milliseconds for a collection of actions.
 *
 * @param {Array.<action.Action>} tickActions
 *     List of actions for one tick.
 *
 * @return {number}
 *     Longest action duration in |tickActions| if any, or 0.
 */
action.computeTickDuration = function(tickActions) {
  let max = 0;
  for (let a of tickActions) {
    let affectsWallClockTime =
      a.subtype == action.Pause ||
      (a.type == "pointer" && a.subtype == action.PointerMove);
    if (affectsWallClockTime && a.duration) {
      max = Math.max(a.duration, max);
    }
  }
  return max;
};

/**
 * Compute viewport coordinates of pointer target based on given origin.
 *
 * @param {action.Action} a
 *     Action that specifies pointer origin and x and y coordinates of target.
 * @param {action.InputState} inputState
 *     Input state that specifies current x and y coordinates of pointer.
 * @param {Map.<string, number>=} center
 *     Object representing x and y coordinates of an element center-point.
 *     This is only used if |a.origin| is a web element reference.
 *
 * @return {Map.<string, number>}
 *     x and y coordinates of pointer destination.
 */
action.computePointerDestination = function(a, inputState, center = undefined) {
  let { x, y } = a;
  switch (a.origin) {
    case action.PointerOrigin.Viewport:
      break;
    case action.PointerOrigin.Pointer:
      x += inputState.x;
      y += inputState.y;
      break;
    default:
      // origin represents web element
      lazy.assert.defined(center);
      lazy.assert.in("x", center);
      lazy.assert.in("y", center);
      x += center.x;
      y += center.y;
  }
  return { x, y };
};

/**
 * Create a closure to use as a map from action definitions to Promise events.
 *
 * @param {number} tickDuration
 *     Duration in milliseconds of this tick.
 * @param {WindowProxy} win
 *     Current window global.
 *
 * @return {function(action.Action): Promise}
 *     Function that takes an action and returns a Promise for dispatching
 *     the event that corresponds to that action.
 */
function toEvents(tickDuration, win) {
  return a => {
    let inputState = action.inputStateMap.get(a.id);

    switch (a.subtype) {
      case action.KeyUp:
        return dispatchKeyUp(a, inputState, win);

      case action.KeyDown:
        return dispatchKeyDown(a, inputState, win);

      case action.PointerDown:
        return dispatchPointerDown(a, inputState, win);

      case action.PointerUp:
        return dispatchPointerUp(a, inputState, win);

      case action.PointerMove:
        return dispatchPointerMove(a, inputState, tickDuration, win);

      case action.PointerCancel:
        throw new lazy.error.UnsupportedOperationError();

      case action.Pause:
        return dispatchPause(a, tickDuration);
    }

    return undefined;
  };
}

/**
 * Dispatch a keyDown action equivalent to pressing a key on a keyboard.
 *
 * @param {action.Action} a
 *     Action to dispatch.
 * @param {action.InputState} inputState
 *     Input state for this action's input source.
 * @param {WindowProxy} win
 *     Current window global.
 *
 * @return {Promise}
 *     Promise to dispatch at least a keydown event, and keypress if
 *     appropriate.
 */
function dispatchKeyDown(a, inputState, win) {
  return new Promise(resolve => {
    let value = a.value;
    if (inputState.shift) {
      value = lazy.event.getShiftedKey(value);
    }

    const keyEvent = new action.Key(value);
    keyEvent.repeat = inputState.isPressed(keyEvent.key);

    inputState.press(keyEvent.key);

    if (keyEvent.key in MODIFIER_NAME_LOOKUP) {
      inputState.setModState(keyEvent.key, true);
    }

    // Append a copy of |a| with keyUp subtype
    action.inputsToCancel.push(Object.assign({}, a, { subtype: action.KeyUp }));
    keyEvent.update(inputState);
    lazy.event.sendKeyDown(keyEvent, win);

    resolve();
  });
}

/**
 * Dispatch a keyUp action equivalent to releasing a key on a keyboard.
 *
 * @param {action.Action} a
 *     Action to dispatch.
 * @param {action.InputState} inputState
 *     Input state for this action's input source.
 * @param {WindowProxy} win
 *     Current window global.
 *
 * @return {Promise}
 *     Promise to dispatch a keyup event.
 */
function dispatchKeyUp(a, inputState, win) {
  return new Promise(resolve => {
    let value = a.value;
    if (inputState.shift) {
      value = lazy.event.getShiftedKey(value);
    }

    const keyEvent = new action.Key(value);

    if (!inputState.isPressed(keyEvent.key)) {
      resolve();
      return;
    }

    if (keyEvent.key in MODIFIER_NAME_LOOKUP) {
      inputState.setModState(keyEvent.key, false);
    }
    inputState.release(keyEvent.key);
    keyEvent.update(inputState);

    lazy.event.sendKeyUp(keyEvent, win);
    resolve();
  });
}

/**
 * Dispatch a pointerDown action equivalent to pressing a pointer-device
 * button.
 *
 * @param {action.Action} a
 *     Action to dispatch.
 * @param {action.InputState} inputState
 *     Input state for this action's input source.
 * @param {WindowProxy} win
 *     Current window global.
 *
 * @return {Promise}
 *     Promise to dispatch at least a pointerdown event.
 */
function dispatchPointerDown(a, inputState, win) {
  return new Promise(resolve => {
    if (inputState.isPressed(a.button)) {
      resolve();
      return;
    }

    inputState.press(a.button);
    // Append a copy of |a| with pointerUp subtype
    let copy = Object.assign({}, a, { subtype: action.PointerUp });
    action.inputsToCancel.push(copy);

    switch (inputState.subtype) {
      case action.PointerType.Mouse:
        let mouseEvent = new action.Mouse("mousedown", a.button);
        mouseEvent.update(inputState);
        if (mouseEvent.ctrlKey) {
          if (lazy.AppInfo.isMac) {
            mouseEvent.button = 2;
            lazy.event.DoubleClickTracker.resetClick();
          }
        } else if (lazy.event.DoubleClickTracker.isClicked()) {
          mouseEvent = Object.assign({}, mouseEvent, { clickCount: 2 });
        }
        lazy.event.synthesizeMouseAtPoint(
          inputState.x,
          inputState.y,
          mouseEvent,
          win
        );
        if (
          lazy.event.MouseButton.isSecondary(a.button) ||
          (mouseEvent.ctrlKey && lazy.AppInfo.isMac)
        ) {
          let contextMenuEvent = Object.assign({}, mouseEvent, {
            type: "contextmenu",
          });
          lazy.event.synthesizeMouseAtPoint(
            inputState.x,
            inputState.y,
            contextMenuEvent,
            win
          );
        }
        break;

      case action.PointerType.Pen:
      case action.PointerType.Touch:
        throw new lazy.error.UnsupportedOperationError(
          "Only 'mouse' pointer type is supported"
        );

      default:
        throw new TypeError(`Unknown pointer type: ${inputState.subtype}`);
    }

    resolve();
  });
}

/**
 * Dispatch a pointerUp action equivalent to releasing a pointer-device
 * button.
 *
 * @param {action.Action} a
 *     Action to dispatch.
 * @param {action.InputState} inputState
 *     Input state for this action's input source.
 * @param {WindowProxy} win
 *     Current window global.
 *
 * @return {Promise}
 *     Promise to dispatch at least a pointerup event.
 */
function dispatchPointerUp(a, inputState, win) {
  return new Promise(resolve => {
    if (!inputState.isPressed(a.button)) {
      resolve();
      return;
    }

    inputState.release(a.button);

    switch (inputState.subtype) {
      case action.PointerType.Mouse:
        let mouseEvent = new action.Mouse("mouseup", a.button);
        mouseEvent.update(inputState);
        if (lazy.event.DoubleClickTracker.isClicked()) {
          mouseEvent = Object.assign({}, mouseEvent, { clickCount: 2 });
        }
        lazy.event.synthesizeMouseAtPoint(
          inputState.x,
          inputState.y,
          mouseEvent,
          win
        );
        break;

      case action.PointerType.Pen:
      case action.PointerType.Touch:
        throw new lazy.error.UnsupportedOperationError(
          "Only 'mouse' pointer type is supported"
        );

      default:
        throw new TypeError(`Unknown pointer type: ${inputState.subtype}`);
    }

    resolve();
  });
}

/**
 * Dispatch a pointerMove action equivalent to moving pointer device
 * in a line.
 *
 * If the action duration is 0, the pointer jumps immediately to the
 * target coordinates.  Otherwise, events are synthesized to mimic a
 * pointer travelling in a discontinuous, approximately straight line,
 * with the pointer coordinates being updated around 60 times per second.
 *
 * @param {action.Action} a
 *     Action to dispatch.
 * @param {action.InputState} inputState
 *     Input state for this action's input source.
 * @param {WindowProxy} win
 *     Current window global.
 *
 * @return {Promise}
 *     Promise to dispatch at least one pointermove event, as well as
 *     mousemove events as appropriate.
 */
function dispatchPointerMove(a, inputState, tickDuration, win) {
  const timer = Cc["@mozilla.org/timer;1"].createInstance(Ci.nsITimer);
  // interval between pointermove increments in ms, based on common vsync
  const fps60 = 17;

  return new Promise((resolve, reject) => {
    const start = Date.now();
    const [startX, startY] = [inputState.x, inputState.y];

    let coords = getElementCenter(a.origin, win);
    let target = action.computePointerDestination(a, inputState, coords);
    const [targetX, targetY] = [target.x, target.y];

    if (!inViewPort(targetX, targetY, win)) {
      throw new lazy.error.MoveTargetOutOfBoundsError(
        `(${targetX}, ${targetY}) is out of bounds of viewport ` +
          `width (${win.innerWidth}) ` +
          `and height (${win.innerHeight})`
      );
    }

    const duration =
      typeof a.duration == "undefined" ? tickDuration : a.duration;
    if (duration === 0) {
      // move pointer to destination in one step
      performOnePointerMove(inputState, targetX, targetY, win);
      resolve();
      return;
    }

    const distanceX = targetX - startX;
    const distanceY = targetY - startY;
    const ONE_SHOT = Ci.nsITimer.TYPE_ONE_SHOT;
    let intermediatePointerEvents = (async () => {
      // wait |fps60| ms before performing first incremental pointer move
      await new Promise(resolveTimer =>
        timer.initWithCallback(resolveTimer, fps60, ONE_SHOT)
      );

      let durationRatio = Math.floor(Date.now() - start) / duration;
      const epsilon = fps60 / duration / 10;
      while (1 - durationRatio > epsilon) {
        let x = Math.floor(durationRatio * distanceX + startX);
        let y = Math.floor(durationRatio * distanceY + startY);
        performOnePointerMove(inputState, x, y, win);
        // wait |fps60| ms before performing next pointer move
        await new Promise(resolveTimer =>
          timer.initWithCallback(resolveTimer, fps60, ONE_SHOT)
        );

        durationRatio = Math.floor(Date.now() - start) / duration;
      }
    })();

    // perform last pointer move after all incremental moves are resolved and
    // durationRatio is close enough to 1
    intermediatePointerEvents
      .then(() => {
        performOnePointerMove(inputState, targetX, targetY, win);
        resolve();
      })
      .catch(err => {
        reject(err);
      });
  });
}

function performOnePointerMove(inputState, targetX, targetY, win) {
  if (targetX == inputState.x && targetY == inputState.y) {
    return;
  }

  switch (inputState.subtype) {
    case action.PointerType.Mouse:
      let mouseEvent = new action.Mouse("mousemove");
      mouseEvent.update(inputState);
      // TODO both pointermove (if available) and mousemove
      lazy.event.synthesizeMouseAtPoint(targetX, targetY, mouseEvent, win);
      break;

    case action.PointerType.Pen:
    case action.PointerType.Touch:
      throw new lazy.error.UnsupportedOperationError(
        "Only 'mouse' pointer type is supported"
      );

    default:
      throw new TypeError(`Unknown pointer type: ${inputState.subtype}`);
  }

  inputState.x = targetX;
  inputState.y = targetY;
}

/**
 * Dispatch a pause action equivalent waiting for `a.duration`
 * milliseconds, or a default time interval of `tickDuration`.
 *
 * @param {action.Action} a
 *     Action to dispatch.
 * @param {number} tickDuration
 *     Duration in milliseconds of this tick.
 *
 * @return {Promise}
 *     Promise that is resolved after the specified time interval.
 */
function dispatchPause(a, tickDuration) {
  let ms = typeof a.duration == "undefined" ? tickDuration : a.duration;
  return lazy.Sleep(ms);
}

// helpers

function capitalize(str) {
  lazy.assert.string(str);
  return str.charAt(0).toUpperCase() + str.slice(1);
}

function inViewPort(x, y, win) {
  lazy.assert.number(x, `Expected x to be finite number`);
  lazy.assert.number(y, `Expected y to be finite number`);
  // Viewport includes scrollbars if rendered.
  return !(x < 0 || y < 0 || x > win.innerWidth || y > win.innerHeight);
}

function getElementCenter(el, win) {
  if (lazy.element.isElement(el)) {
    if (action.specCompatPointerOrigin) {
      return lazy.element.getInViewCentrePoint(el.getClientRects()[0], win);
    }
    return lazy.element.coordinates(el);
  }
  return {};
}
