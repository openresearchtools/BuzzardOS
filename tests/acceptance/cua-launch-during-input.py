#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-or-later
"""Guest-only regression: independent input, then cross-seat app launches.

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
        expected_files = {1: "", 2: ""}
        for index in (1, 2):
            file = output / f"editor-{index}.txt"
            file.touch()
            launched = call(index, "launch_app", {"name": "env", "additional_arguments": [
                "GDK_BACKEND=wayland",
                f"XDG_CONFIG_HOME={output / f'config-{index}'}",
                f"XDG_CACHE_HOME={output / f'cache-{index}'}",
                "mousepad", "--disable-server", str(file),
            ]})
            if launched["code"]:
                raise RuntimeError("test editor did not launch; see report.json")
            data = json.loads(launched["stdout"])
            targets[index] = {"pid": data["pid"], "window_id": data["windows"][0]["window_id"]}

        report["initial_tree"] = json.loads(subprocess.check_output(
            ["swaymsg", "-r", "-t", "get_tree"], text=True, timeout=5))

        # First establish two independent native input streams. Different
        # strings and pointer coordinates expose cross-routing that identical
        # actions on both seats would hide. Window launches are a separate
        # phase: their focus-policy failures must not mask this baseline.
        started = time.monotonic()
        human_focus = next(seat["focus"] for seat in seats() if seat["name"] == "seat0")
        with ThreadPoolExecutor(max_workers=2) as pool:
            pending = []
            for index, marker in ((1, "ALPHA bookkeeper 1122"), (2, "bravo COFFEE 9988")):
                expected_files[index] = "".join(f"{marker}: line {line}\n" for line in range(12))
                pending.append(pool.submit(call, index, "type_text", {
                    **targets[index], "text": expected_files[index]}))
            typed = [task.result() for task in pending]
        for index in (1, 2):
            call(index, "press_key", {**targets[index], "key": "s", "modifiers": ["CTRL"]})
        report["concurrent_input"] = {
            "exact": all((output / f"editor-{index}.txt").read_text() == expected_files[index]
                         for index in (1, 2)),
            "codes": [record["code"] for record in typed],
            "overlap_seconds": max(0, min(record["finished"] for record in typed)
                                   - max(record["started"] for record in typed)),
            "human_focus_unchanged": all(
                "error" not in sample and next(seat["focus"] for seat in sample["seats"]
                                               if seat["name"] == "seat0") == human_focus
                for sample in report["samples"] if sample["time"] >= started),
        }
        with ThreadPoolExecutor(max_workers=2) as pool:
            moves = [pool.submit(call, index, "move_cursor", {"x": x, "y": y, "scope": "desktop"})
                     for index, x, y in ((1, 137, 211), (2, 491, 337))]
            moved = [move.result() for move in moves]
        positions = [call(index, "get_cursor_position", {}) for index in (1, 2)]
        moved.append(call(1, "move_cursor", {"x": 173, "y": 229, "scope": "desktop"}))
        unchanged = call(2, "get_cursor_position", {})
        expected_positions = [(137, 211), (491, 337)]
        positions_match = all(
            record["code"] == 0 and
            (json.loads(record["stdout"]).get("x"), json.loads(record["stdout"]).get("y")) == expected
            for record, expected in zip(positions, expected_positions))
        report["pointer_independence"] = {
            "positions": positions,
            "moves_succeeded": not any(record["code"] for record in moved),
            "positions_match": positions_match,
            "other_seat_unchanged": (positions[1]["code"] == unchanged["code"] == 0
                                     and positions[1]["stdout"] == unchanged["stdout"]),
        }
        for index in (1, 2):
            capture = subprocess.run([
                f"cua{index}", "screenshot", "{}", "--screenshot-out-file",
                str(output / f"cua{index}.png")], capture_output=True, text=True, timeout=30)
            report[f"screenshot_{index}"] = {
                "code": capture.returncode, "stdout": capture.stdout, "stderr": capture.stderr}
        persist()

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
            expected_files[typing] += text
            report["cases"].append({
                "typing_seat": typing, "launching_seat": launching,
                "expected": expected_files[typing], "actual": actual,
                "exact": expected_files[typing] == actual,
                "codes": [typed["code"], launched["code"], saved["code"]],
                "overlap_seconds": max(0, min(typed["finished"], launched["finished"])
                                       - max(typed["started"], launched["started"])),
            })
            persist()
            if expected_files[typing] != actual:
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
            and report["concurrent_input"]["exact"]
            and not any(report["concurrent_input"]["codes"])
            and report["concurrent_input"]["overlap_seconds"] > 0
            and report["concurrent_input"]["human_focus_unchanged"]
            and report["pointer_independence"]["other_seat_unchanged"]
            and report["pointer_independence"]["moves_succeeded"]
            and report["pointer_independence"]["positions_match"]
            and all(report[f"screenshot_{index}"]["code"] == 0 for index in (1, 2))
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
