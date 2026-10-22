#!/usr/bin/env python3
"""Collect real Quest/host counters without mistaking HUD estimates for optical latency."""
import argparse
import datetime
import hashlib
import ipaddress
import json
import math
import os
from pathlib import Path
import re
import signal
import shlex
import statistics
import subprocess
import time

PACKAGE = "com.linuxquest.client"
PROPERTIES = ("debug.linuxquest.host", "debug.linuxquest.transport", "debug.linuxquest.metrics")


def run(*args, check=True):
    return subprocess.run(args, text=True, stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
                          timeout=15, check=check).stdout.strip()


def proc(pid):
    # comm may contain spaces and parentheses. Fields after the final ')' start at field 3.
    fields = Path(f"/proc/{pid}/stat").read_text().rsplit(")", 1)[1].split()
    return int(fields[11]) + int(fields[12]), fields[19]


def distribution(values):
    if not values:
        return None
    ordered = sorted(values)
    return {"n": len(values), "mean": statistics.mean(values), "min": min(values),
            "p95": ordered[math.ceil(len(values) * .95) - 1], "max": max(values)}


def summarize(samples, logs, client_pid):
    metrics = []
    vrapi = []
    for line in logs.splitlines():
        # Never attribute vrshell / another application's VrApi counters to this client.
        match = re.match(r"\s*(\d+\.\d+)\s+(\d+)\s+", line)
        if not match or match[2] != client_pid:
            continue
        stamp = float(match[1])
        if not samples or not samples[0]["epoch"] <= stamp <= samples[-1]["epoch"]:
            continue
        if "LQ_METRICS " in line:
            payload = line.split("LQ_METRICS ", 1)[1]
            try:
                value, _ = json.JSONDecoder().raw_decode(payload)
                metrics.append((stamp, value))
            except (ValueError, json.JSONDecodeError):
                pass
        match = re.search(r"FPS=(\d+)/(\d+).*?Stale=(\d+)", line)
        if match:
            vrapi.append({"fps": int(match[1]), "hz": int(match[2]), "stale": int(match[3])})
    result = {"cpu_one_core_percent": distribution([s["cpu_one_core_percent"] for s in samples]),
              "cpu_machine_percent": distribution([s["cpu_machine_percent"] for s in samples]),
              "gpu_device_percent": distribution([s["gpu"][0] for s in samples if s.get("gpu")]),
              "gpu_encoder_percent": distribution([s["gpu"][1] for s in samples if s.get("gpu")]),
              "client_metric_samples": len(metrics), "vrapi_samples": len(vrapi),
              "vrapi_fps": distribution([v["fps"] for v in vrapi]),
              "vrapi_stale_sum": sum(v["stale"] for v in vrapi) if vrapi else None,
              "optical_latency_ms": None,
              "optical_latency_status": "NOT MEASURED: requires optical apparatus",
              "stream": None}
    if len(metrics) >= 2:
        first, last = metrics[0][1], metrics[-1][1]
        elapsed = metrics[-1][0] - metrics[0][0]
        keys = ("loop_frames", "queued", "decoded", "surface_releases", "decoder_dropped",
                "packets", "packets_lost", "fec_recovered")
        reset = any(b[1][k] < a[1][k] for a, b in zip(metrics, metrics[1:]) for k in keys)
        delta = {k: last[k] - first[k] for k in keys}
        result["stream"] = {"seconds": elapsed, "counter_reset": reset,
                            "delta": delta, "last": last,
                            "decoded_fps": delta["decoded"] / elapsed if elapsed > 0 and not reset else None,
                            "surface_release_fps": delta["surface_releases"] / elapsed if elapsed > 0 and not reset else None,
                            "hud_estimate_ms": distribution([m["hud_estimate_ms"] for _, m in metrics]),
                            "decode_last_us": distribution([m["decode_last_us"] for _, m in metrics])}
    return result


def process_gpu(log, pid):
    """pmon '-' is unavailable, never zero utilization."""
    columns = ("sm", "memory", "encoder", "decoder")
    values = {k: [] for k in columns}
    for line in log.splitlines():
        fields = line.split()
        if len(fields) >= 8 and fields[0].isdigit() and fields[1] == str(pid):
            for key, value in zip(columns, fields[3:7]):
                if value.isdigit():
                    values[key].append(float(value))
    return {key + "_percent": distribution(value) for key, value in values.items()}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--serial", required=True)
    parser.add_argument("--host-pid", type=int, required=True)
    parser.add_argument("--mode", choices=("usb", "wifi"), required=True)
    parser.add_argument("--host-ip", help="LAN IP of host, required for Wi-Fi")
    parser.add_argument("--seconds", type=int, default=60)
    parser.add_argument("--warmup", type=int, default=10)
    parser.add_argument("--output", type=Path, required=True, help="New directory for raw evidence and report")
    parser.add_argument("--host-log", type=Path, help="Log of this host, to record negotiated geometry")
    parser.add_argument("--observe", action="store_true",
                        help="Read-only: do not launch/stop apps, change properties, or manage ADB tunnels")
    args = parser.parse_args()
    if args.seconds < 3 or args.warmup < 0 or (args.mode == "wifi" and not args.host_ip and not args.observe):
        parser.error("need seconds >= 3, warmup >= 0, and host-ip for Wi-Fi")
    if args.host_ip:
        try:
            ipaddress.ip_address(args.host_ip)
        except ValueError:
            parser.error("host-ip must be an IP address")
    if "linux-quest-host" not in Path(f"/proc/{args.host_pid}/exe").resolve().name:
        parser.error("host-pid must identify linux-quest-host")
    args.output.mkdir(parents=True, exist_ok=False)
    adb = lambda *a, **kw: run("adb", "-s", args.serial, *a, **kw)

    def apk_hash():
        path = adb("shell", "pm", "path", PACKAGE).splitlines()[0].removeprefix("package:")
        data = subprocess.check_output(["adb", "-s", args.serial, "exec-out", "cat", shlex.quote(path)], timeout=15)
        return hashlib.sha256(data).hexdigest()
    if adb("get-state") != "device":
        raise RuntimeError("Quest not authorized")
    initial = {p: adb("shell", "getprop", p) for p in PROPERTIES}
    reverse = adb("reverse", "--list")
    # Preserve every pre-existing reverse rule; refuse to replace another destination.
    mapping = next((line.split()[2] for line in reverse.splitlines()
                    if len(line.split()) == 3 and line.split()[1] == "tcp:48444"), None)
    if args.mode == "usb" and not args.observe and mapping not in (None, "tcp:48444"):
        raise RuntimeError("tcp:48444 already forwards elsewhere")
    metadata = {"utc": datetime.datetime.now(datetime.timezone.utc).isoformat(),
                "commit": run("git", "rev-parse", "HEAD"),
                "dirty": run("git", "status", "--porcelain"),
                "mode": args.mode, "host_pid": args.host_pid,
                "logical_cpus": os.cpu_count(), "kernel": run("uname", "-sr"),
                "host_exe_sha256": hashlib.sha256(Path(f"/proc/{args.host_pid}/exe").read_bytes()).hexdigest(),
                "gpu": run("nvidia-smi", "--query-gpu=name,driver_version", "--format=csv,noheader", check=False),
                "model": adb("shell", "getprop", "ro.product.model"),
                "fingerprint": adb("shell", "getprop", "ro.build.fingerprint"),
                "seconds_requested": args.seconds, "warmup_seconds": args.warmup}
    metadata["observe_only"] = args.observe
    metadata["host_command"] = Path(f"/proc/{args.host_pid}/cmdline").read_bytes().decode().split("\0")[:-1]
    metadata["host_cwd"] = str(Path(f"/proc/{args.host_pid}/cwd").resolve())
    metadata["apk_path_before"] = adb("shell", "pm", "path", PACKAGE)
    metadata["apk_sha256_before"] = apk_hash()
    capture = None
    gpu_capture = None
    gpu_log = None
    samples = []
    log = None
    try:
        if args.mode == "usb" and not args.observe:
            adb("reverse", "tcp:48444", "tcp:48444")
        if not args.observe:
            for prop, value in zip(PROPERTIES, (args.host_ip if args.mode == "wifi" else "127.0.0.1",
                                                "udp" if args.mode == "wifi" else "usb", "1")):
                adb("shell", "setprop", prop, shlex.quote(value))
            adb("shell", "am", "force-stop", PACKAGE)
        log = (args.output / "logcat.txt").open("w")
        capture = subprocess.Popen(["adb", "-s", args.serial, "logcat", "-v", "epoch", "-T", "1"],
                                   stdout=log, stderr=subprocess.STDOUT)
        if not args.observe:
            adb("shell", "am", "start", "-W", "-n", f"{PACKAGE}/android.app.NativeActivity")
        time.sleep(args.warmup)
        client_pid = adb("shell", "pidof", PACKAGE)
        if not client_pid.isdigit():
            raise RuntimeError("client did not remain running")
        metadata["client_pid"] = client_pid
        # Align only the collection windows, not the two devices' monotonic frame clocks.
        before = time.time()
        device_epoch = float(adb("shell", "date", "+%s.%N"))
        after = time.time()
        offset = device_epoch - (before + after) / 2
        metadata["logcat_clock_offset_seconds"] = offset
        metadata["clock_query_half_rtt_seconds"] = (after - before) / 2
        print(f"Collecting {args.mode} for {args.seconds}s; client PID {client_pid}", flush=True)
        ticks, identity = proc(args.host_pid)
        last_time = time.monotonic()
        hz = os.sysconf("SC_CLK_TCK")
        gpu_log = (args.output / "gpu-pmon.txt").open("w")
        gpu_capture = subprocess.Popen(["nvidia-smi", "pmon", "-i", "0", "-s", "u", "-d", "1"],
                                       stdout=gpu_log, stderr=subprocess.STDOUT)
        with (args.output / "samples.jsonl").open("w") as raw:
            for _ in range(args.seconds):
                time.sleep(1)
                current, same = proc(args.host_pid)
                now = time.monotonic()
                if same != identity:
                    raise RuntimeError("host PID reused")
                cpu = (current - ticks) / hz / (now - last_time) * 100
                gpu_text = run("nvidia-smi", "--query-gpu=utilization.gpu,utilization.encoder,utilization.decoder",
                               "--format=csv,noheader,nounits", "--id=0", check=False)
                try:
                    gpu = [float(x) for x in gpu_text.split(",")]
                except ValueError:
                    gpu = None
                row = {"epoch": time.time() + offset, "cpu_one_core_percent": cpu,
                       "cpu_machine_percent": cpu / os.cpu_count(), "gpu": gpu}
                samples.append(row)
                raw.write(json.dumps(row) + "\n")
                raw.flush()
                ticks, last_time = current, now
        metadata["client_pid_after"] = adb("shell", "pidof", PACKAGE, check=False)
        metadata["apk_path_after"] = adb("shell", "pm", "path", PACKAGE, check=False)
        metadata["apk_sha256_after"] = apk_hash()
        metadata["power"] = adb("shell", "dumpsys", "power")
        # Retain Wi-Fi capability only, without SSID, MAC, IP, or nearby network inventories.
        wifi = adb("shell", "dumpsys", "wifi", check=False)
        wifi_info = next((line for line in wifi.splitlines() if "mWifiInfo " in line), "")
        metadata["wifi_link"] = list(dict.fromkeys(re.findall(
            r"Wi-Fi standard: \d+|RSSI: -?\d+|(?:Tx |Rx )?Link speed: \d+Mbps|Frequency: \d+MHz", wifi_info)))
    except BaseException as error:
        (args.output / "report.md").write_text(
            f"# Hardware run interrupted\n\n{type(error).__name__}: {error}\n\n"
            "All performance targets remain NOT VALIDATED. Inspect raw evidence before retrying.\n")
        raise
    finally:
        if gpu_capture:
            gpu_capture.terminate()
            gpu_capture.wait(timeout=10)
        if gpu_log:
            gpu_log.close()
        if capture:
            capture.terminate()
            capture.wait(timeout=10)
        if log:
            log.close()
        if not args.observe:
            for prop, value in initial.items():
                # adb joins shell arguments; explicitly quote empty values to clear properties.
                adb("shell", "setprop", prop, shlex.quote(value), check=False)
            if args.mode == "usb" and mapping is None:
                adb("reverse", "--remove", "tcp:48444", check=False)
            adb("shell", "input", "keyevent", "KEYCODE_HOME", check=False)
        (args.output / "metadata.json").write_text(json.dumps(metadata, indent=2) + "\n")
    if args.host_log:
        (args.output / "host.log").write_bytes(args.host_log.read_bytes())
    result = summarize(samples, (args.output / "logcat.txt").read_text(errors="replace"), client_pid)
    result["host_gpu"] = process_gpu((args.output / "gpu-pmon.txt").read_text(), args.host_pid)
    result["client_pid_stable"] = metadata["client_pid_after"] == client_pid
    result["apk_path_stable"] = metadata["apk_path_before"] == metadata["apk_path_after"]
    result["apk_bytes_stable"] = metadata["apk_sha256_before"] == metadata["apk_sha256_after"]
    (args.output / "summary.json").write_text(json.dumps(result, indent=2) + "\n")
    stream = result["stream"]
    video_status = "NOT VALIDATED: exported counters unavailable"
    if stream:
        video_status = ("OBSERVED" if not stream["counter_reset"] and stream["delta"]["surface_releases"] > 0
                        and not stream["last"]["simulated"] and result["client_pid_stable"]
                        and result["apk_bytes_stable"] else "FAIL: no valid live video surface release interval")
    report = [f"# Hardware run: {args.mode}", "", f"UTC: {metadata['utc']}", "",
              "| Measurement | Raw summary |", "| --- | --- |"]
    for key, value in result.items():
        report.append(f"| {key} | `{json.dumps(value, sort_keys=True)}` |")
    report += ["", "| Target | Result |", "| --- | --- |",
               "| Optical latency <10 ms USB / <15 ms Wi-Fi | NOT MEASURED; optical equipment required |",
               "| Host CPU <5% at 4K 90 Hz | NOT VALIDATED automatically; verify negotiated geometry AND sustained decoded rate |",
               f"| End-to-end video | {video_status} |",
               "| 1:1 text readability | NOT VALIDATED; requires in-headset visual inspection |",
               "| Resolution/reconnect/sleep-wake reliability | NOT VALIDATED by this stationary collection |", "",
               "CPU is process CPU: 100% = one logical core; machine percent divides by logical CPU count.",
               "GPU utilization is for the entire NVIDIA device, including other applications.",
               "host_gpu is PID-filtered NVIDIA pmon telemetry; unavailable '-' values remain null.",
               "HUD Est is a software estimate, not optical latency. Surface releases are not photons.",
               "Decoder drops omit packets lost before decoding; packet loss and compositor stale frames are separate.",
               "VrApi FPS/Stale come only from the client PID. Stale sum is a sum of periodic runtime reports.",
               "Raw evidence: samples.jsonl, logcat.txt, metadata.json, summary.json and optional host.log."]
    (args.output / "report.md").write_text("\n".join(report) + "\n")
    print(json.dumps(result, indent=2))
    return 0 if (stream and not stream["counter_reset"] and not stream["last"]["simulated"]
                 and result["client_pid_stable"] and result["apk_path_stable"]
                 and result["apk_bytes_stable"]
                 and stream["delta"]["surface_releases"] > 0) else 2


if __name__ == "__main__":
    def interrupted(*_):
        raise KeyboardInterrupt()

    signal.signal(signal.SIGTERM, interrupted)
    raise SystemExit(main())
