#!/usr/bin/env python3
"""Check that the chart renders [server] link_origin (ADR-0058).

The key is the operator's one explicit origin for links to Search. It stays
out of trawld.toml until config.server.linkOrigin is set, and a set value
renders as the operator wrote it: the chart does not validate or rewrite it,
trawld refuses a bad origin at load. The chart renders offline through Helm,
with no cluster.
"""

import unittest

from query_scratch import objects, server_config


class LinkOrigin(unittest.TestCase):
    def test_default_render_sets_no_link_origin(self):
        self.assertNotIn("link_origin", server_config(objects())["server"])

    def test_empty_value_sets_no_link_origin(self):
        rendered = objects({"config": {"server": {"linkOrigin": ""}}})
        self.assertNotIn("link_origin", server_config(rendered)["server"])

    def test_set_value_renders_in_the_server_table(self):
        rendered = objects({"config": {"server": {"linkOrigin": "https://trawl.example.com:8443"}}})
        self.assertEqual(
            server_config(rendered)["server"]["link_origin"], "https://trawl.example.com:8443",
        )


if __name__ == "__main__":
    unittest.main()
