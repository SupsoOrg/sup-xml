// Thin C ABI over pugixml so the head_to_head bench can call it from Rust.
//
// pugixml is C++-only. We expose three symbols:
//
//   pugixml_bench_parse(buf, len)  -> opaque doc pointer, or NULL on failure
//   pugixml_bench_walk(doc)        -> count of element nodes (forces tree walk
//                                     so the parse can't be optimised away)
//   pugixml_bench_free(doc)        -> release the document
//
// The bench measures `parse + walk + free` to mirror what libxml2 and
// roxmltree runners do (parse a buffer and count descendants).

#include <pugixml.hpp>

#include <cstddef>

extern "C" {

void* pugixml_bench_parse(const char* buf, std::size_t len) {
    auto* doc = new pugi::xml_document();
    pugi::xml_parse_result r = doc->load_buffer(
        buf, len,
        pugi::parse_default,            // expand 5 builtin entities, normalise newlines,
                                        // CDATA -> text, attr ws conv.
        pugi::encoding_utf8);
    if (!r) {
        delete doc;
        return nullptr;
    }
    return doc;
}

// Walk every node and tally element count. Equivalent work to
// roxmltree's `descendants().count()` in the head_to_head bench.
static std::size_t walk(const pugi::xml_node& n) {
    std::size_t c = (n.type() == pugi::node_element) ? 1 : 0;
    for (auto child = n.first_child(); child; child = child.next_sibling()) {
        c += walk(child);
    }
    return c;
}

std::size_t pugixml_bench_walk(void* doc) {
    auto* d = static_cast<pugi::xml_document*>(doc);
    return walk(*d);
}

void pugixml_bench_free(void* doc) {
    delete static_cast<pugi::xml_document*>(doc);
}

}  // extern "C"
