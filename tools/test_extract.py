import unittest
import extract


class PageCoverageTests(unittest.TestCase):
    def test_every_printed_page_must_be_present_once_in_order(self):
        extract.check_pages([{"page": 1}, {"page": 2}], 2)
        for pages in [[{"page": 1}], [{"page": 1}, {"page": 1}],
                      [{"page": 2}, {"page": 1}], [{"page": 0}, {"page": 1}]]:
            with self.assertRaises(ValueError):
                extract.check_pages(pages, 2)

    def test_manifest_paths_cannot_select_another_folder(self):
        for value in ["", ".", "..", "C:foo", "../other.pdf", "a/b.pdf", "a\\b.pdf"]:
            with self.assertRaises(ValueError):
                extract.safe_file(value)
        self.assertEqual(extract.safe_file("001-source.pdf"), "001-source.pdf")


if __name__ == "__main__":
    unittest.main()
