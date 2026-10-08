"""Rules that check documentation source pages."""
import re

# A bearer token expanded by the shell lands on curl's command line, where
# other local users can read it. Pages send keys through a curl config file.
# Any shell expansion after the scheme or curl's --oauth2-bearer counts:
# $(...), backticks, $NAME and ${NAME}, quoted or not.
TOKEN_ON_ARGV = re.compile(r'\bbearer[\s=]+["\']*[$`]', re.IGNORECASE)
