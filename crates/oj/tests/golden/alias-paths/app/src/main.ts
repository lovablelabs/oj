import debug from "debug";
import { shout } from "#lib/format";
import { shout as shoutViaPaths } from "@app/lib/format";
import { whisper } from "@app/lib/quiet";

const log = debug("golden");
log(shout("hello"), shoutViaPaths("again"), whisper("HUSH"));
import("@app/lib/later").then((m) => log(m.later()));
