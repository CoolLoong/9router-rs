#!/usr/bin/env python3
import json
import os
from pathlib import Path
import sys

name = Path(sys.argv[0]).name
args = sys.argv[1:]
ledger = Path(os.environ["DATA_DIR"]) / "local-device-tts-ledger.jsonl"


def record(**values):
    with ledger.open("a") as output:
        output.write(json.dumps({"executable":name, **values}, ensure_ascii=False) + "\n")


if name == "say":
    if args == ["-v", "?"]:
        record(operation="voices")
        print("Fixture Voice  en_US # synthetic")
        print("声 音  zh_CN # synthetic")
        print(" invalid voice  ja_JP")
        raise SystemExit(0)
    model = None
    if args[:1] == ["-v"]:
        model = args[1]
        args = args[2:]
    output = Path(args[1])
    assert args[0] == "-o" and output.name == "out.aiff" and output.parent.name.startswith("tts-")
    text_args = args[2:]
    protected = text_args[:1] == ["--"]
    if protected:
        text_args = text_args[1:]
    assert len(text_args) == 1
    text = text_args[0]
    record(operation="render", model=model, text=text, protected=protected, directory=str(output.parent))
    if text == "Fixture say failure":
        raise SystemExit(4)
    output.write_text(json.dumps({"voice":model,"text":text}, ensure_ascii=False))
    raise SystemExit(0)

if name == "ffmpeg":
    assert len(args) == 8 and args[:2] == ["-y", "-i"] and args[3:7] == ["-codec:a", "libmp3lame", "-qscale:a", "4"]
    aiff, result = Path(args[2]), Path(args[-1])
    assert aiff.name == "out.aiff" and result.name == "out.mp3" and aiff.parent == result.parent
    info = json.loads(aiff.read_text())
    record(operation="encode", directory=str(result.parent))
    if info["text"] == "Fixture ffmpeg failure":
        raise SystemExit(17)
    result.write_bytes(b"fixture-mp3\x00" + f'{info["voice"]}:{info["text"]}'.encode())
    raise SystemExit(0)

raise SystemExit(3)
