"""Unit tests for source_rules."""
import unittest

from source_rules import TOKEN_ON_ARGV


class TokenOnArgv(unittest.TestCase):
    def test_shell_expansions_are_refused(self):
        for line in ('-H "Authorization: Bearer $(cat alice.token)"',
                     '-H "Authorization: Bearer $TOKEN"',
                     '-H "Authorization: Bearer ${TOKEN}"',
                     "-H 'Authorization: Bearer '\"$TOKEN\"",
                     '-H "Authorization: Bearer "$TOKEN',
                     '-H "authorization: bearer $TOKEN"',
                     '-H "Authorization: Bearer `cat alice.token`"',
                     '--oauth2-bearer "$TOKEN"',
                     '--oauth2-bearer=$TOKEN',
                     '--oauth2-bearer="$(cat alice.token)"',
                     '-H "Authorization: Bearer flt_${TOKEN}"',
                     '-H "Authorization: Bearer prefix$TOKEN"'):
            self.assertRegex(line, TOKEN_ON_ARGV)

    def test_literal_and_config_forms_pass(self):
        for line in ('Authorization: Bearer flt_...',
                     "printf 'header = \"Authorization: Bearer %s\"\\n' \"$(cat alice.token)\"",
                     'Bearer-token authentication',
                     'a bearer token'):
            self.assertNotRegex(line, TOKEN_ON_ARGV)


if __name__ == '__main__':
    unittest.main()
