"""Unit tests for the empty-Markdown check in rendered_page.Page."""
import unittest

from rendered_page import Page


def empty(region):
    return Page(f'<main><div class="sl-markdown-content">{region}</div></main>').empty_markdown


class EmptyMarkdown(unittest.TestCase):
    def test_no_children_is_empty(self):
        self.assertTrue(empty(''))

    def test_whitespace_is_empty(self):
        self.assertTrue(empty('\n  \n'))

    def test_empty_nested_elements_are_empty(self):
        self.assertTrue(empty('<p></p>'))
        self.assertTrue(empty('<div><p> </p></div>'))

    def test_void_elements_do_not_hide_an_empty_region(self):
        self.assertTrue(empty('<br>'))
        self.assertTrue(empty('<p><br><wbr></p>'))

    def test_text_in_non_visible_elements_is_empty(self):
        self.assertTrue(empty('<script>init()</script>'))
        self.assertTrue(empty('<style>p { color: red }</style>'))
        self.assertTrue(empty('<template><p>later</p><img src="a.png"></template>'))

    def test_text_beside_a_script_is_content(self):
        self.assertFalse(empty('<script>init()</script><p>Install the package.</p>'))

    def test_text_is_content(self):
        self.assertFalse(empty('<p>Install the package.</p>'))

    def test_media_is_content(self):
        self.assertFalse(empty('<p><img src="a.png" alt=""></p>'))
        self.assertFalse(empty('<svg viewBox="0 0 1 1"></svg>'))

    def test_text_after_a_void_element_is_content(self):
        self.assertFalse(empty('<p><br>text</p>'))

    def test_page_without_a_markdown_region_is_not_empty(self):
        self.assertFalse(Page('<main><p></p></main>').empty_markdown)


if __name__ == '__main__':
    unittest.main()
