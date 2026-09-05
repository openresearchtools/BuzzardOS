#!/usr/bin/python3
# SPDX-License-Identifier: AGPL-3.0-or-later
import asyncio
from pathlib import Path
import runpy
import unittest
from unittest.mock import AsyncMock, Mock, patch

ROOT = Path(__file__).resolve().parents[2]
AGENT = runpy.run_path(str(ROOT / "guest/assets/buzzardos-integration-agent"))
MediaEndpoint = AGENT["MediaEndpoint"]
GLOBALS = MediaEndpoint.reconcile.__globals__


class MediaIntegrationTests(unittest.IsolatedAsyncioTestCase):
    async def test_failed_channel_does_not_prevent_other_listeners(self):
        endpoints = [MediaEndpoint(name, port) for name, port in AGENT["MEDIA_PORTS"].items()]
        microphone = Mock(pid=12)
        camera = Mock(pid=13)
        microphone.poll.return_value = camera.poll.return_value = None
        with patch.dict(GLOBALS, {
            "start_pipeline": Mock(side_effect=[OSError("audio unavailable"), microphone, camera]),
            "wait_for_listener": AsyncMock(),
        }):
            await asyncio.gather(*(endpoint.reconcile() for endpoint in endpoints))
        self.assertFalse(endpoints[0].status()["running"])
        self.assertIn("audio unavailable", endpoints[0].error)
        self.assertTrue(endpoints[1].status()["running"])
        self.assertTrue(endpoints[2].status()["running"])

    async def test_disconnect_retries_only_that_endpoint(self):
        endpoint = MediaEndpoint("host_camera", 47132)
        old = Mock(pid=12)
        old.poll.return_value = 1
        new = Mock(pid=13)
        new.poll.return_value = None
        endpoint.process = old
        stop = Mock()
        start = Mock(return_value=new)
        with patch.dict(GLOBALS, {"stop_pipeline": stop, "start_pipeline": start, "wait_for_listener": AsyncMock()}):
            await endpoint.reconcile()
            start.assert_not_called()
            stop.assert_called_once_with(old)
            endpoint.retry_at = 0
            await endpoint.reconcile()
            await endpoint.reconcile()
            start.assert_called_once()
        self.assertIs(endpoint.process, new)
        self.assertIsNone(endpoint.error)

    async def test_listener_start_failure_stops_owned_process(self):
        endpoint = MediaEndpoint("host_microphone", 47131)
        child = Mock(pid=12)
        stop = Mock()
        with patch.dict(GLOBALS, {
            "start_pipeline": Mock(return_value=child),
            "stop_pipeline": stop,
            "wait_for_listener": AsyncMock(side_effect=RuntimeError("not listening")),
        }):
            await endpoint.reconcile()
        stop.assert_called_once_with(child)
        self.assertIsNone(endpoint.process)
        self.assertEqual(endpoint.error, "not listening")

    def test_host_and_guest_declare_required_gdp_plugin_package(self):
        build = (ROOT / "packaging/build-debs.sh").read_text()
        dependencies = [line for line in build.splitlines() if "gstreamer1.0-pipewire," in line]
        self.assertEqual(len(dependencies), 2)
        for line in dependencies:
            self.assertIn("gstreamer1.0-plugins-bad,", line)


if __name__ == "__main__":
    unittest.main()
