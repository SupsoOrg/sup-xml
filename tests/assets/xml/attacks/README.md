# XML Attack Test Files

Pathological and malicious XML inputs that have historically caused problems in
parsers. Each file targets a specific class of bug; filenames encode the attack.

Parsers should either reject these with a clear error, or impose limits that
prevent resource exhaustion. None of these should crash, hang, OOM, or leak
sensitive data.

## Categories

### Entity expansion (DoS)
- `billion_laughs.xml` — classic exponential entity expansion
- `billion_laughs_deep.xml` — same idea, more levels
- `billion_laughs_utf8.xml` — non-ASCII payload to test byte-counting
- `quadratic_blowup.xml` — single huge entity referenced many times
- `entity_expansion_in_attribute.xml` — entity expansion inside an attribute value (must trip the budget)
- `parameter_entity_recursion.xml` — `%a;` -> `%b;` -> `%a;`
- `parameter_entity_self.xml` — parameter entity references itself
- `nested_general_entity_cycle.xml` — general entity cycle

### External entity / XXE
- `xxe_file_read.xml` — `SYSTEM "file:///etc/passwd"`
- `xxe_file_read_param.xml` — parameter-entity variant
- `xxe_http_ssrf.xml` — `SYSTEM "http://..."` for SSRF
- `xxe_oob_dtd.xml` — out-of-band exfil via external DTD
- `xxe_php_filter.xml` — `php://filter` base64 wrapper trick
- `xxe_expect.xml` — `expect://id` command execution wrapper
- `xxe_netdoc.xml` — `netdoc:` / `jar:` scheme tricks

### XInclude
- `xinclude_file.xml` — XInclude pulling /etc/passwd
- `xinclude_recursive.xml` — XInclude that includes itself
- `xinclude_http.xml` — XInclude over HTTP (SSRF)
- `xinclude_xpointer_bomb.xml` — XPointer expression DoS

### Structural depth
- `deep_nesting_1k.xml` — 1,000 nested elements
- `deep_nesting_10k.xml` — 10,000 nested elements
- `deep_nesting_100k.xml` — 100,000 nested elements (stack killer)
- `deep_mixed_content.xml` — deep nesting with text at every level

### Attribute / namespace pathologies
- `many_attributes.xml` — single element with 100,000 attributes
- `duplicate_attributes.xml` — same attribute repeated
- `huge_attribute_value.xml` — single attribute value 10 MiB
- `attribute_name_collision_hash.xml` — names crafted to collide in common hashes
- `namespace_prefix_explosion.xml` — thousands of namespace declarations
- `namespace_redefinition.xml` — same prefix rebound many times
- `xmlns_empty_uri.xml` — `xmlns:foo=""` (illegal in XML 1.0)

### Lexical / scanner
- `unterminated_comment.xml`
- `unterminated_cdata.xml`
- `unterminated_pi.xml`
- `unterminated_tag.xml`
- `nested_comment.xml` — `<!-- <!-- -->` (illegal)
- `comment_double_hyphen.xml` — `--` inside comment (illegal)
- `cdata_in_attribute.xml` — CDATA where it's not allowed
- `pi_xml_target.xml` — `<?XML ...?>` (case variant, illegal as target)
- `bom_only.xml` — file is just a BOM
- `empty_document.xml` — zero bytes
- `whitespace_only.xml`
- `trailing_garbage.xml` — content after root close
- `multiple_roots.xml` — two top-level elements
- `prolog_after_root.xml` — `<?xml ...?>` after content
- `mismatched_tags.xml`
- `unclosed_root.xml`

### Encoding
- `utf16_no_bom.xml` — UTF-16 declared, no BOM
- `utf8_bom_wrong_decl.xml` — UTF-8 BOM but declares UTF-16
- `utf7_encoding.xml` — UTF-7 (historical XSS vector)
- `mixed_encoding.xml` — header says UTF-8, body has Latin-1 bytes
- `invalid_utf8_overlong.xml` — overlong UTF-8 encoding of ASCII
- `invalid_utf8_surrogate.xml` — UTF-8 encoding of a UTF-16 surrogate
- `null_byte.xml` — embedded NUL inside text
- `utf16_surrogate_lone.xml` — lone surrogate in UTF-16

### Character references
- `charref_above_unicode.xml` — `&#x110000;`
- `charref_surrogate.xml` — `&#xD800;`
- `charref_nul.xml` — `&#0;`
- `charref_control.xml` — `&#x01;` (illegal control char)
- `charref_huge_number.xml` — `&#9999999999999;`
- `entity_undefined.xml` — `&undefined;`
- `entity_only_predefined.xml` — only `&lt; &gt; &amp; &apos; &quot;`

### Long names / huge tokens
- `long_element_name.xml` — element name 1 MiB
- `long_attribute_name.xml`
- `long_pi_target.xml`
- `long_text_node.xml`
- `long_comment.xml`

### DTD pathologies
- `dtd_external_only.xml` — references external DTD that may not exist
- `dtd_circular_includes.xml`
- `dtd_attlist_huge.xml` — ATTLIST with many enumerated values
- `dtd_notation_redefine.xml`
- `dtd_with_pe_in_internal_subset.xml` — PE in internal subset (illegal)

### XML 1.0 vs 1.1
- `xml11_c0_controls.xml` — C0 controls allowed in XML 1.1 only
- `xml10_with_nel.xml` — NEL char treated differently in 1.0 vs 1.1

### Whitespace and special
- `xml_space_preserve_nested.xml`
- `xml_base_redefinition.xml`
- `default_attr_namespace_interaction.xml`
