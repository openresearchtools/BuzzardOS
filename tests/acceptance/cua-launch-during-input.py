#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-or-later
"""Guest-only regression: one CUA types while the other launches an app.

Run as the graphical guest user with its Wayland/Sway environment. Requires
Mousepad, Foot and the installed CUA package. No host input, shell restart,
production instrumentation or application process kill is used. Test windows
and evidence are intentionally retained for inspection.
"""

import argparse
from concurrent.futures import ThreadPoolExecutor
import json
from pathlib import Path
import subprocess
import threading
import time


def run(output: Path) -> bool:
    output.mkdir(parents=True, exist_ok=False)
    report = {"calls": [], "samples": [], "cases": []}
    lock = threading.Lock()
    stop = threading.Event()

    def persist():
        with lock:
            (output / "report.json").write_text(json.dumps(report, indent=2))

    def seats():
        result = subprocess.run(
            ["swaymsg", "-r", "-t", "get_seats"], capture_output=True,
            text=True, timeout=5, check=True,
        )
        return json.loads(result.stdout)

    def call(index, tool, arguments):
        started = time.monotonic()
        result = subprocess.run(
            [f"cua{index}", tool, json.dumps(arguments)], capture_output=True,
            text=True, timeout=40,
        )
        record = {"index": index, "tool": tool, "arguments": arguments,
                  "started": started, "finished": time.monotonic(),
                  "code": result.returncode, "stdout": result.stdout,
                  "stderr": result.stderr}
        with lock:
            report["calls"].append(record)
        persist()
        return record

    def monitor():
        while not stop.is_set():
            try:
                sample = {"time": time.monotonic(), "seats": seats()}
            except Exception as error:
                sample = {"time": time.monotonic(), "error": str(error)}
            with lock:
                report["samples"].append(sample)
            stop.wait(.02)

    report["baseline"] = seats()
    human = next(seat for seat in report["baseline"] if seat["name"] == "seat0")
    sampler = threading.Thread(target=monitor)
    sampler.start()
    try:
        targets = {}
        for index in (1, 2):
            file = output / f"editor-{index}.txt"
            file.touch()
            launched = call(index, "launch_app", {"name": "env", "additional_arguments": [
                f"XDG_CONFIG_HOME={output / f'config-{index}'}",
                f"XDG_CACHE_HOME={output / f'cache-{index}'}",
                "mousepad", "--disable-server", str(file),
            ]})
            if launched["code"]:
                raise RuntimeError("test editor did not launch; see report.json")
            data = json.loads(launched["stdout"])
            targets[index] = {"pid": data["pid"], "window_id": data["windows"][0]["window_id"]}

        for number, typing in enumerate((1, 2)):
            launching = 3 - typing
            target = targets[typing]
            text = "".join(
                f"CUA{typing} independent line {line}: AaBBcc bookkeeper coffee 112233!\n"
                for line in range(6)
            )

            def launch_later():
                time.sleep(.8)
                return call(launching, "launch_app", {"name": "foot", "additional_arguments": [
                    "--title", f"Other-seat launch {number}", "cat",
                ]})

            with ThreadPoolExecutor(max_workers=2) as pool:
                typed = pool.submit(call, typing, "type_text", {**target, "text": text})
                launched = pool.submit(launch_later)
                typed, launched = typed.result(), launched.result()
            saved = call(typing, "press_key", {**target, "key": "s", "modifiers": ["CTRL"]})
            actual = (output / f"editor-{typing}.txt").read_text()
            report["cases"].append({
                "typing_seat": typing, "launching_seat": launching,
                "expected": text, "actual": actual, "exact": text == actual,
                "codes": [typed["code"], launched["code"], saved["code"]],
                "overlap_seconds": max(0, min(typed["finished"], launched["finished"])
                                       - max(typed["started"], launched["started"])),
            })
            persist()
            if text != actual:
                break
    except Exception as error:
        report["error"] = str(error)
    finally:
        stop.set()
        sampler.join()
        report["final_seats"] = seats()
        report["human_unchanged"] = all(
            "error" not in sample and
            next(seat for seat in sample["seats"] if seat["name"] == "seat0") == human
            for sample in report["samples"]
        )
        report["pass"] = (
            "error" not in report and report["human_unchanged"]
            and len(report["cases"]) == 2
            and all(case["exact"] and not any(case["codes"]) and case["overlap_seconds"] > 0
                    for case in report["cases"])
        )
        persist()
    print(json.dumps({"pass": report["pass"], "report": str(output / "report.json")}))
    return report["pass"]


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", type=Path, required=True, help="New guest test-evidence directory")
    options = parser.parse_args()
    raise SystemExit(0 if run(options.output.resolve()) else 1)
