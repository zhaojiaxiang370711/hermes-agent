#!/usr/bin/env python3
"""
boxingAgent 语音运行时 - 真实麦克风测试

用法：
    python3 voice_test.py              # 完整测试（唤醒 + ASR）
    python3 voice_test.py --asr-only   # 跳过唤醒，直接测试 ASR
    python3 voice_test.py --record 15  # 录 15 秒（默认 10）

依赖：pip install sounddevice numpy

前置条件：
    boxing-agent voice  # 启动语音运行时（sherpa-onnx 模式）
"""

import argparse
import base64
import json
import os
import shutil
import subprocess
import threading
import time
import urllib.request
import wave
from queue import Empty, Queue

import numpy as np
import sounddevice as sd

VOICE_URL = os.environ.get("VOICE_URL", "http://127.0.0.1:50058")
SAMPLE_RATE = 48000
RUNTIME_SAMPLE_RATE = 16000
PRINTED_SOURCES = set()


def is_real_input(name):
    low = name.lower()
    return "monitor" not in low and not low.startswith("alsa_output")


def list_input_devices():
    for i, d in enumerate(sd.query_devices()):
        if d["max_input_channels"] > 0:
            suffix = "" if is_real_input(d["name"]) else "  (monitor/输出监听，不推荐)"
            print(
                f"{i:>3}  {d['name']}  inputs={d['max_input_channels']} "
                f"sr={int(d['default_samplerate'])}{suffix}"
            )
    sources = pulse_sources()
    if sources:
        print("\nPulse/PipeWire sources:")
        for name in sources:
            suffix = "" if is_real_input(name) else "  (monitor/输出监听，不推荐)"
            print(f"  {name}{suffix}")


def find_mic(preferred=None):
    if preferred is not None:
        return preferred
    devices = list(enumerate(sd.query_devices()))
    for i, d in devices:
        name = d["name"].lower()
        if "ugreen" in name and "alsa_input" in name and d["max_input_channels"] > 0:
            return i
    for i, d in devices:
        if d["max_input_channels"] > 0 and is_real_input(d["name"]):
            return i
    return sd.default.device[0]


def pulse_sources():
    if not shutil.which("pactl"):
        return []
    try:
        out = subprocess.check_output(
            ["pactl", "list", "short", "sources"],
            text=True,
            stderr=subprocess.DEVNULL,
        )
    except Exception:
        return []
    sources = []
    for line in out.splitlines():
        parts = line.split()
        if len(parts) >= 2:
            sources.append(parts[1])
    return sources


def find_pulse_source(preferred=None):
    if preferred:
        return preferred
    sources = pulse_sources()
    for source in sources:
        low = source.lower()
        if "ugreen" in low and "alsa_input" in low and "monitor" not in low:
            return source
    for source in sources:
        if is_real_input(source):
            return source
    return None


def reset_runtime():
    try:
        urllib.request.urlopen(
            urllib.request.Request(
                f"{VOICE_URL}/api/v1/voice-runtime/reset", method="POST"
            ),
            timeout=5,
        )
        return True
    except Exception:
        return False


def simulate_wake():
    try:
        urllib.request.urlopen(
            urllib.request.Request(
                f"{VOICE_URL}/api/v1/voice-runtime/simulate/wake",
                data=json.dumps({"keyword": "小星", "confidence": 0.95}).encode(),
                headers={"Content-Type": "application/json"},
            ),
            timeout=5,
        )
        return True
    except Exception:
        return False


def record_audio(duration, mic_device):
    """录制麦克风音频，返回 numpy 数组（48kHz i16）"""
    audio = sd.rec(
        int(duration * SAMPLE_RATE),
        samplerate=SAMPLE_RATE,
        channels=1,
        dtype="int16",
        device=mic_device,
    )
    sd.wait()
    return audio


def record_pcm_parec(duration, source=None):
    if not shutil.which("parec"):
        raise RuntimeError("parec not found; install pulseaudio-utils or use --backend sounddevice")
    source = find_pulse_source(source)
    label = f"{source or 'default'} (parec)"
    if label not in PRINTED_SOURCES:
        print(f"🎤 采集源: {label}")
        PRINTED_SOURCES.add(label)
    last_error = None
    for attempt in range(1, 4):
        cmd = [
            "timeout",
            str(max(1, int(duration))),
            "parec",
            "--format=s16le",
            f"--rate={RUNTIME_SAMPLE_RATE}",
            "--channels=1",
        ]
        if source:
            cmd.append(f"--device={source}")
        raw_path = f"/tmp/boxing-voice-{os.getpid()}-{int(time.time() * 1000)}-{attempt}.raw"
        try:
            os.unlink(raw_path)
        except OSError:
            pass
        cmd.append(raw_path)
        result = subprocess.run(cmd, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE, check=False)
        try:
            # timeout returns 124 after intentionally stopping parec.
            if result.returncode not in (0, 124):
                stderr = result.stderr.decode(errors="ignore").strip()
                raise RuntimeError(f"parec failed with exit code {result.returncode}: {stderr}")
            if not os.path.exists(raw_path):
                raise RuntimeError("parec did not create an output file")
            data = np.fromfile(raw_path, dtype=np.int16)
            if data.size > 0:
                return data
            last_error = RuntimeError("parec returned empty audio")
        except Exception as exc:
            last_error = exc
        finally:
            try:
                os.unlink(raw_path)
            except OSError:
                pass
        if attempt < 3:
            print("  ⚠️  parec 本次为空，重试...")
            time.sleep(0.3)
    raise last_error or RuntimeError("parec failed")


def record_pcm_parecord(duration, source=None):
    if not shutil.which("parecord"):
        raise RuntimeError("parecord not found; install pulseaudio-utils")
    source = find_pulse_source(source)
    label = f"{source or 'default'} (parecord)"
    if label not in PRINTED_SOURCES:
        print(f"🎤 采集源: {label}")
        PRINTED_SOURCES.add(label)
    last_error = None
    for attempt in range(1, 4):
        raw_path = f"/tmp/boxing-voice-{os.getpid()}-{int(time.time() * 1000)}-{attempt}.wav"
        try:
            os.unlink(raw_path)
        except OSError:
            pass
        cmd = [
            "timeout",
            str(max(2, int(duration) + 1)),
            "parecord",
            "--format=s16le",
            f"--rate={RUNTIME_SAMPLE_RATE}",
            "--channels=1",
        ]
        if source:
            cmd.append(f"--device={source}")
        cmd.append(raw_path)
        result = subprocess.run(cmd, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE, check=False)
        try:
            if result.returncode not in (0, 124):
                stderr = result.stderr.decode(errors="ignore").strip()
                raise RuntimeError(f"parecord failed with exit code {result.returncode}: {stderr}")
            if not os.path.exists(raw_path):
                raise RuntimeError("parecord did not create an output file")
            with wave.open(raw_path, "rb") as wav:
                if wav.getsampwidth() != 2:
                    raise RuntimeError(f"unexpected sample width: {wav.getsampwidth()}")
                data = np.frombuffer(wav.readframes(wav.getnframes()), dtype=np.int16).copy()
            if data.size > 0:
                return data
            last_error = RuntimeError("parecord returned empty audio")
        except Exception as exc:
            last_error = exc
        finally:
            try:
                os.unlink(raw_path)
            except OSError:
                pass
        if attempt < 3:
            print("  ⚠️  parecord 本次为空，重试...")
            time.sleep(0.3)
    raise last_error or RuntimeError("parecord failed")


def resample_48_to_16(audio):
    """48kHz → 16kHz 简单下采样"""
    return audio.reshape(-1, 3).mean(axis=1).astype(np.int16)


def record_pcm(duration, backend="auto", device=None, source=None):
    if backend in ("auto", "parecord") and shutil.which("parecord"):
        return record_pcm_parecord(duration, source=source), RUNTIME_SAMPLE_RATE
    if backend in ("auto", "parec") and shutil.which("parec"):
        return record_pcm_parec(duration, source=source), RUNTIME_SAMPLE_RATE

    mic = find_mic(device)
    mic_name = sd.query_devices(mic)["name"]
    print(f"🎤 麦克风: [{mic}] {mic_name} (sounddevice)")
    if not is_real_input(mic_name):
        print("⚠️  当前设备像是输出监听 monitor，不是真实麦克风；建议用 --device 指定 alsa_input 设备")
    audio = record_audio(duration, mic)
    return resample_48_to_16(audio), RUNTIME_SAMPLE_RATE


def send_pcm_chunks(pcm_i16, sr=16000, chunk_ms=500):
    """分块发送 PCM 到语音运行时"""
    chunk_size = int(sr * chunk_ms / 1000)
    for i in range(0, len(pcm_i16), chunk_size):
        chunk = pcm_i16[i : i + chunk_size]
        if len(chunk) == 0:
            continue
        b64 = base64.b64encode(chunk.tobytes()).decode()
        try:
            urllib.request.urlopen(
                urllib.request.Request(
                    f"{VOICE_URL}/api/v1/voice-runtime/audio/pcm",
                    data=json.dumps(
                        {"pcm16_base64": b64, "sample_rate": sr}
                    ).encode(),
                    headers={"Content-Type": "application/json"},
                ),
                timeout=10,
            )
        except Exception:
            pass
        time.sleep(0.02)

    # final_chunk
    urllib.request.urlopen(
        urllib.request.Request(
            f"{VOICE_URL}/api/v1/voice-runtime/audio/pcm",
            data=json.dumps(
                {"pcm16_base64": "", "sample_rate": sr, "final_chunk": True}
            ).encode(),
            headers={"Content-Type": "application/json"},
        ),
        timeout=10,
    )


def post_pcm(pcm_i16, sr=16000, final_chunk=False):
    b64 = base64.b64encode(pcm_i16.tobytes()).decode()
    urllib.request.urlopen(
        urllib.request.Request(
            f"{VOICE_URL}/api/v1/voice-runtime/audio/pcm",
            data=json.dumps(
                {"pcm16_base64": b64, "sample_rate": sr, "final_chunk": final_chunk}
            ).encode(),
            headers={"Content-Type": "application/json"},
        ),
        timeout=10,
    )


def post_final(sr=16000):
    urllib.request.urlopen(
        urllib.request.Request(
            f"{VOICE_URL}/api/v1/voice-runtime/audio/pcm",
            data=json.dumps({"pcm16_base64": "", "sample_rate": sr, "final_chunk": True}).encode(),
            headers={"Content-Type": "application/json"},
        ),
        timeout=10,
    )


def play_audio(path):
    """Best-effort playback for generated TTS files."""
    players = [
        ["ffplay", "-nodisp", "-autoexit", "-loglevel", "quiet", path],
        ["mpv", "--really-quiet", path],
        ["mpg123", "-q", path],
    ]
    for cmd in players:
        if shutil.which(cmd[0]):
            try:
                subprocess.run(cmd, check=False)
                return True
            except Exception:
                return False
    print("  🔇 未找到播放器，可手动打开音频文件")
    return False


def listen_events(duration, play=False):
    """监听 SSE 事件，返回事件列表"""
    events = []
    stop = threading.Event()

    def _listen():
        try:
            req = urllib.request.Request(f"{VOICE_URL}/api/v1/voice-runtime/events")
            resp = urllib.request.urlopen(req, timeout=duration + 5)
            for line in resp:
                if stop.is_set():
                    break
                line = line.decode().strip()
                if line.startswith("data:"):
                    ev = json.loads(line[5:].strip())
                    events.append(ev)
                    t = ev.get("type")
                    if t == "wake_word":
                        kw = ev.get("keyword", "?")
                        conf = ev.get("confidence", "?")
                        print(f"  🔔 唤醒词: '{kw}' (置信度 {conf})")
                    elif t == "transcript" and ev.get("final_result"):
                        print(f"  📝 转写: {ev.get('transcript')}")
                    elif t == "assistant_start":
                        print("  🤖 大模型处理中...")
                    elif t == "assistant_reply":
                        text = ev.get("assistant_text") or ""
                        print(f"  💬 回复: {text}")
                        media = ev.get("media")
                        file_path = ev.get("file_path")
                        if media:
                            print(f"  🔊 音频: {media}")
                        if play and file_path:
                            play_audio(file_path)
                    elif t == "assistant_error":
                        print(f"  ❌ 对话失败: {ev.get('message')}")
        except Exception:
            pass

    thread = threading.Thread(target=_listen, daemon=True)
    thread.start()
    return events, stop, thread


def start_event_listener(play=False):
    events = Queue()
    stop = threading.Event()

    def _listen():
        while not stop.is_set():
            try:
                req = urllib.request.Request(f"{VOICE_URL}/api/v1/voice-runtime/events")
                resp = urllib.request.urlopen(req, timeout=30)
                for line in resp:
                    if stop.is_set():
                        break
                    line = line.decode().strip()
                    if line.startswith("data:"):
                        ev = json.loads(line[5:].strip())
                        events.put(ev)
                        t = ev.get("type")
                        if t == "wake_word":
                            print(f"\n\a🔔 唤醒成功: {ev.get('keyword') or '?'}")
                            print("🎙️  我在听，请说话...")
                        elif t == "transcript" and ev.get("final_result"):
                            print(f"📝 转写: {ev.get('transcript')}")
                        elif t == "assistant_start":
                            print("🤖 大模型处理中...")
                        elif t == "assistant_reply":
                            text = ev.get("assistant_text") or ""
                            print(f"💬 回复: {text}")
                            media = ev.get("media")
                            file_path = ev.get("file_path")
                            if media:
                                print(f"🔊 音频: {media}")
                            if play and file_path:
                                play_audio(file_path)
                        elif t == "assistant_error":
                            print(f"❌ 对话失败: {ev.get('message')}")
            except Exception:
                if not stop.is_set():
                    time.sleep(0.5)

    thread = threading.Thread(target=_listen, daemon=True)
    thread.start()
    return events, stop, thread


def drain_events(q):
    drained = []
    while True:
        try:
            drained.append(q.get_nowait())
        except Empty:
            return drained


def run_continuous(command_seconds, backend, device, source, play=False):
    """常驻监听：持续送小块音频给 KWS；唤醒后收一段指令再触发 ASR/对话。"""
    print("🎧 常驻监听中。说「小星」唤醒，按 Ctrl+C 退出。")
    print(f"   唤醒后会收音 {command_seconds} 秒作为本轮指令。")
    reset_runtime()
    events, stop, thread = start_event_listener(play=play)
    chunk_seconds = 2
    capturing = False
    capture_deadline = 0.0
    waiting_for_reply = False
    try:
        while True:
            try:
                pcm, sr = record_pcm(chunk_seconds, backend=backend, device=device, source=source)
                peak = int(np.abs(pcm).max()) if len(pcm) else 0
                if not capturing and not waiting_for_reply:
                    print(f"\r等待唤醒... 音量峰值 {peak:<5}", end="", flush=True)
                post_pcm(pcm, sr=sr, final_chunk=False)
            except Exception as exc:
                print(f"\n⚠️  音频采集/发送失败: {exc}")
                time.sleep(0.5)
                continue

            for ev in drain_events(events):
                t = ev.get("type")
                if t == "wake_word" and not capturing and not waiting_for_reply:
                    capturing = True
                    capture_deadline = time.time() + command_seconds
                elif t in ("assistant_reply", "assistant_error"):
                    waiting_for_reply = False
                    reset_runtime()
                    print("\n🎧 已回到等待唤醒。")

            if capturing and time.time() >= capture_deadline:
                capturing = False
                waiting_for_reply = True
                print("\n📡 指令收音结束，正在识别...")
                try:
                    post_final(sr=RUNTIME_SAMPLE_RATE)
                except Exception as exc:
                    print(f"⚠️  final_chunk 发送失败: {exc}")
                    waiting_for_reply = False
                    reset_runtime()
    finally:
        stop.set()
        thread.join(timeout=1)


def check_runtime():
    """检查语音运行时是否可用"""
    try:
        resp = urllib.request.urlopen(f"{VOICE_URL}/health", timeout=3)
        d = json.loads(resp.read())
        return d.get("healthy", False)
    except Exception:
        return False


def get_status():
    """获取运行时状态"""
    try:
        resp = urllib.request.urlopen(f"{VOICE_URL}/api/v1/voice-runtime/status", timeout=3)
        return json.loads(resp.read())
    except Exception:
        return None


def main():
    parser = argparse.ArgumentParser(description="boxingAgent 语音运行时测试")
    parser.add_argument("--continuous", action="store_true", help="常驻监听唤醒词并循环对话")
    parser.add_argument("--asr-only", action="store_true", help="跳过唤醒词，直接测试 ASR")
    parser.add_argument("--record", type=int, default=10, help="录音秒数（默认 10）")
    parser.add_argument("--command-seconds", type=int, default=6, help="唤醒后收音秒数（默认 6）")
    parser.add_argument("--play", action="store_true", help="收到 TTS 音频后尝试播放")
    parser.add_argument("--backend", choices=("auto", "parecord", "parec", "sounddevice"), default="auto", help="录音后端")
    parser.add_argument("--device", type=int, help="sounddevice 输入设备编号")
    parser.add_argument("--source", help="Pulse/PipeWire source name for parec")
    parser.add_argument("--list-devices", action="store_true", help="列出可用输入设备后退出")
    args = parser.parse_args()

    if args.list_devices:
        list_input_devices()
        return

    # 检查运行时
    if not check_runtime():
        print("❌ 语音运行时未启动")
        print("   请先运行: boxing-agent voice")
        print("   或设置 QXZN_VOICE_RUNTIME_ENGINE=sherpa-onnx 后启动")
        return

    status = get_status()
    if status:
        engine = status.get("engine", "?")
        kw_model = status.get("kws_model_exists", False)
        asr_model = status.get("asr_model_configured", False)
        print(f"✅ 语音运行时已启动 (引擎: {engine})")
        print(f"   唤醒词模型: {'✅' if kw_model else '❌'}  ASR模型: {'✅' if asr_model else '❌'}")

    if args.continuous:
        run_continuous(
            args.command_seconds,
            backend=args.backend,
            device=args.device,
            source=args.source,
            play=args.play,
        )
        return

    duration = args.record
    print(f"⏱️  录音 {duration} 秒\n")

    # 重置运行时
    reset_runtime()

    if args.asr_only:
        # 跳过唤醒，直接模拟唤醒后 ASR
        print("🔔 模拟唤醒（跳过唤醒词检测）...")
        simulate_wake()
    else:
        print("📢 请对着麦克风说「小星」，然后说一句话")
        print("   （如果唤醒词检测不到，会自动用模拟唤醒测试 ASR）\n")

    # 启动 SSE 监听
    events, stop, thread = listen_events(duration + 8, play=args.play)

    # 录音
    print("🔴 录音中...")
    pcm, sr = record_pcm(duration, backend=args.backend, device=args.device, source=args.source)
    print(f"✅ 录音完成 ({len(pcm)} 样本 @ {sr}Hz, 峰值 {int(np.abs(pcm).max())})")

    # 发送
    print("📡 发送 PCM 到语音运行时...")
    send_pcm_chunks(pcm, sr=sr)

    # 等待事件
    time.sleep(6)
    stop.set()
    thread.join(timeout=1)

    # 汇总
    wakes = [e for e in events if e.get("type") == "wake_word"]
    transcripts = [e for e in events if e.get("type") == "transcript" and e.get("final_result")]
    replies = [e for e in events if e.get("type") == "assistant_reply"]

    if wakes and transcripts:
        print(f"\n✅ 完整流程成功！")
        print(f"   唤醒词: '{wakes[0].get('keyword')}' (置信度 {wakes[0].get('confidence')})")
        print(f"   转写: {transcripts[0].get('transcript')}")
        if replies:
            print(f"   回复: {replies[-1].get('assistant_text')}")
    elif transcripts:
        print(f"\n✅ ASR 转写成功: {transcripts[0].get('transcript')}")
        if replies:
            print(f"   回复: {replies[-1].get('assistant_text')}")
    elif not args.asr_only:
        print(f"\n⚠️  未检测到唤醒词（请说「小星」）")
        print(f"   尝试用 --asr-only 跳过唤醒词，直接测试 ASR：")
        print(f"   python3 voice_test.py --asr-only --record {duration}")


if __name__ == "__main__":
    main()
