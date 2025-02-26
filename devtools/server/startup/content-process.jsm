/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

"use strict";

/*
 * Module that listens for requests to start a `DevToolsServer` for an entire content
 * process.  Loaded into content processes by the main process during
 * content-process-connector.js' `connectToContentProcess` via the process
 * script `content-process.js`.
 *
 * The actual server startup itself is in this JSM so that code can be cached.
 */

/* exported initContentProcessTarget */
const EXPORTED_SYMBOLS = ["initContentProcessTarget"];

function initContentProcessTarget(msg) {
  const mm = msg.target;
  const prefix = msg.data.prefix;
  const watcherActorID = msg.data.watcherActorID;

  // Lazy load Loader.jsm to prevent loading any devtools dependency too early.
  const {
    useDistinctSystemPrincipalLoader,
    releaseDistinctSystemPrincipalLoader,
  } = ChromeUtils.import("resource://devtools/shared/loader/Loader.jsm");

  // Use a unique object to identify this one usage of the loader
  const loaderRequester = {};

  // Init a custom, invisible DevToolsServer, in order to not pollute the
  // debugger with all devtools modules, nor break the debugger itself with
  // using it in the same process.
  const loader = useDistinctSystemPrincipalLoader(loaderRequester);

  const { DevToolsServer } = loader.require("devtools/server/devtools-server");

  DevToolsServer.init();
  // For browser content toolbox, we do need a regular root actor and all tab
  // actors, but don't need all the "browser actors" that are only useful when
  // debugging the parent process via the browser toolbox.
  DevToolsServer.registerActors({ root: true, target: true });

  // Connect both parent/child processes devtools servers RDP via message
  // managers
  const conn = DevToolsServer.connectToParent(prefix, mm);
  conn.parentMessageManager = mm;

  const { ContentProcessTargetActor } = loader.require(
    "devtools/server/actors/targets/content-process"
  );

  const actor = new ContentProcessTargetActor(conn, {
    sessionContext: msg.data.sessionContext,
  });
  actor.manage(actor);

  const response = { watcherActorID, prefix, actor: actor.form() };
  mm.sendAsyncMessage("debug:content-process-actor", response);

  // Clean up things when the client disconnects
  mm.addMessageListener("debug:content-process-disconnect", function onDestroy(
    message
  ) {
    if (message.data.prefix != prefix) {
      // Several copies of this process script can be running for a single process if
      // we are debugging the same process from multiple clients.
      // If this disconnect request doesn't match a connection known here, ignore it.
      return;
    }
    mm.removeMessageListener("debug:content-process-disconnect", onDestroy);

    // Call DevToolsServerConnection.close to destroy all child actors. It should end up
    // calling DevToolsServerConnection.onTransportClosed that would actually cleanup all actor
    // pools.
    conn.close();
  });

  // Destroy the related loader when the target is destroyed
  // and we were the last user of the special loader
  actor.once("destroyed", () => {
    releaseDistinctSystemPrincipalLoader(loaderRequester);
  });

  return {
    actor,
    connection: conn,
  };
}
