#!/usr/bin/env python3
"""Exercise attribution and missing/reset counters without a headset."""
import unittest

from hw_validate import process_gpu, summarize


class Summaries(unittest.TestCase):
    samples = [{"epoch": 100, "cpu_one_core_percent": 24, "cpu_machine_percent": 1,
                "gpu": [15, 10, 0]},
               {"epoch": 103, "cpu_one_core_percent": 48, "cpu_machine_percent": 2,
                "gpu": [20, 12, 0]}]

    def metric(self, stamp, count, pid=42):
        import json
        payload = dict.fromkeys(("loop_frames", "queued", "decoded", "surface_releases",
                                 "decoder_dropped", "packets", "packets_lost", "fec_recovered"), count)
        payload.update(hud_estimate_ms=20, decode_last_us=6000, simulated=False)
        return f'{stamp:.3f} {pid} 43 I RustStdoutStderr: LQ_METRICS {json.dumps(payload)}'

    def test_rates_and_other_app_excluded(self):
        logs = "\n".join([self.metric(100, 0), self.metric(102, 120), self.metric(101, 999, 99),
                          "101.000 99 43 I VrApi: FPS=72/72,Stale=50",
                          "101.000 42 43 I VrApi: FPS=90/90,Stale=2"])
        result = summarize(self.samples, logs, "42")
        self.assertEqual(result["stream"]["decoded_fps"], 60)
        self.assertEqual(result["vrapi_stale_sum"], 2)
        self.assertEqual(result["cpu_one_core_percent"]["mean"], 36)
        self.assertIsNone(result["optical_latency_ms"])

    def test_no_samples_is_unknown_not_zero(self):
        result = summarize([], "", "42")
        self.assertIsNone(result["stream"])
        self.assertIsNone(result["vrapi_stale_sum"])
        self.assertIsNone(result["cpu_one_core_percent"])

    def test_counter_reset_invalidates_rate_even_if_final_count_larger(self):
        logs = "\n".join([self.metric(100, 10), self.metric(101, 0), self.metric(102, 50)])
        result = summarize(self.samples, logs, "42")
        self.assertTrue(result["stream"]["counter_reset"])
        self.assertIsNone(result["stream"]["decoded_fps"])

    def test_outside_measurement_window_excluded(self):
        result = summarize(self.samples, self.metric(99, 0) + "\n" + self.metric(104, 900), "42")
        self.assertEqual(result["client_metric_samples"], 0)

    def test_process_gpu_unavailable_is_not_zero(self):
        result = process_gpu("0 42 C 5 6 - 0 - - host\n0 99 C 80 80 80 80 - - other", 42)
        self.assertEqual(result["sm_percent"]["mean"], 5)
        self.assertEqual(result["decoder_percent"]["mean"], 0)
        self.assertIsNone(result["encoder_percent"])


if __name__ == "__main__":
    unittest.main()
