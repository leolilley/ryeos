# rye:signed:2026-02-21T05:56:40Z:44c33f812e022d5857b37e73c50a4671f7a8fc20525c3cbfd83157919776a090:-72tlH0WlluWeExwEKNbGXfx3Lwj1ePJ92ndg-vQqV-KwkfHxpxOQX_0KiLUPbSUYYKXFHXFtxM1trxdFxgrBg==:9fbfabe975fa5a7f
"""Markdown XML parser for directives.

Handles extraction of XML from markdown code fences and parsing
with support for opaque sections (template, example, raw tags).
"""

__version__ = "1.1.0"
__tool_type__ = "parser"
__category__ = "rye/core/parsers"
__tool_description__ = (
    "Markdown XML parser - extracts and parses XML from markdown code fences"
)

import re
import xml.etree.ElementTree as ET
from typing import Any, Dict, Optional, Tuple

from rye.constants import Action

PRIMARY_ACTIONS = frozenset(Action.ALL)


def parse(content: str) -> Dict[str, Any]:
    """Parse directive markdown with embedded XML.

    Returns parsed directive data with all metadata.
    """
    # Extract XML from markdown fence
    xml_str, preamble, body = _extract_xml_block(content)
    if xml_str is None:
        return {
            "body": content,
            "raw": content,
            "data": {},
            "error": "No XML code block found",
        }

    try:
        # Parse basic attributes first (before masking)
        directive_match = re.match(r"<directive\s+([^>]*)>", xml_str)
        result: Dict[str, Any] = {}

        if directive_match:
            attrs_str = directive_match.group(1)
            # Extract name
            name_match = re.search(r'name\s*=\s*["\']([^"\']+)["\']', attrs_str)
            if name_match:
                result["name"] = name_match.group(1)
            # Extract version
            version_match = re.search(r'version\s*=\s*["\']([^"\']+)["\']', attrs_str)
            if version_match:
                result["version"] = version_match.group(1)

        # Mask opaque sections before parsing
        masked_xml, opaque_content = _mask_opaque_sections(xml_str)

        # Parse XML
        try:
            root = ET.fromstring(masked_xml)
        except ET.ParseError as e:
            return {
                **result,
                "preamble": preamble,
                "body": body,
                "raw": content,
                "error": f"Invalid XML: {e}",
            }

        # Extract structured data
        _extract_from_xml(root, result)

        # Reattach opaque content
        result["templates"] = opaque_content
        result["preamble"] = preamble
        result["body"] = body
        result["raw"] = content
        result["content"] = xml_str

        return result

    except Exception as e:
        return {
            "body": body if "body" in locals() else content,
            "raw": content,
            "data": {},
            "error": str(e),
        }


def _extract_xml_block(content: str) -> Tuple[Optional[str], str, str]:
    """Extract XML from markdown ```xml ... ``` block.

    Returns (xml_content, preamble, body) where:
      - preamble: markdown text before the XML fence (title, summary)
      - body: everything after the closing fence — free-form LLM prompt
    """
    # Match ```xml not inside an outer fence (````markdown etc.)
    outer_open_pat = re.compile(r"^`{4,}", re.MULTILINE)
    outer_close_pat = re.compile(r"^`{4,}\s*$", re.MULTILINE)

    for match in re.finditer(r"^```xml\s*$", content, re.MULTILINE):
        before = content[: match.start()]
        if len(outer_open_pat.findall(before)) > len(outer_close_pat.findall(before)):
            continue  # Inside an outer fence — skip

        preamble = before.strip()
        start = match.end() + 1

        # Find closing ```
        close_match = re.search(r"^```\s*$", content[start:], re.MULTILINE)
        if close_match:
            xml_content = content[start : start + close_match.start()].strip()
            after_fence = start + close_match.end()
            body = content[after_fence:].strip()
            return xml_content, preamble, body

    return None, "", ""


def _mask_opaque_sections(xml_str: str) -> Tuple[str, Dict[str, str]]:
    """Mask opaque tag sections before XML parsing.

    Prevents parsing errors from template/example content.
    """
    opaque_tags = {"template", "example", "raw"}
    masked = xml_str
    opaque_content: Dict[str, str] = {}
    counter = 0

    for tag in opaque_tags:
        # Find all <tag>...</tag> patterns
        pattern = f"<{tag}[^>]*>(.*?)</{tag}>"
        for match in re.finditer(pattern, masked, re.DOTALL):
            placeholder = f"__opaque_{tag}_{counter}__"
            opaque_content[placeholder] = match.group(1)
            masked = masked.replace(
                match.group(0), f'<{tag} data-masked-id="{placeholder}"></{tag}>'
            )
            counter += 1

    return masked, opaque_content


def _extract_from_xml(root: ET.Element, result: Dict[str, Any]) -> None:
    """Extract all metadata from parsed XML tree."""

    # Extract description (can be at root or in metadata)
    desc_elem = root.find("description")
    if desc_elem is not None and desc_elem.text:
        result["description"] = desc_elem.text.strip()

    # Extract metadata section
    metadata_elem = root.find("metadata")
    if metadata_elem is not None:
        for child in metadata_elem:
            tag = child.tag

            # Handle model tag specially - extract attributes
            if tag == "model" or tag == "model_class":
                result["model"] = dict(child.attrib)

            # Handle permissions - parse nested permission elements
            elif tag == "permissions":
                permissions = []
                perm_text = (child.text or "").strip()
                if perm_text == "*" and len(child) == 0:
                    permissions.append({"tag": "cap", "content": "rye.*"})
                else:
                    for perm in child:
                        if perm.tag not in PRIMARY_ACTIONS:
                            continue
                        inner_text = (perm.text or "").strip()
                        if inner_text == "*" and len(perm) == 0:
                            permissions.append({"tag": "cap", "content": f"rye.{perm.tag}.*"})
                        elif len(perm) > 0:
                            for item in perm:
                                item_text = (item.text or "").strip()
                                if item_text:
                                    permissions.append({"tag": "cap", "content": f"rye.{perm.tag}.{item.tag}.{item_text}"})
                        elif inner_text:
                            permissions.append({"tag": "cap", "content": f"rye.{perm.tag}.{inner_text}"})
                result["permissions"] = permissions

            elif tag == "limits":
                limits = {}
                for k, v in child.attrib.items():
                    try:
                        if '.' in v:
                            limits[k] = float(v)
                        else:
                            limits[k] = int(v)
                    except ValueError:
                        limits[k] = v
                result["limits"] = limits

            elif tag == "hooks":
                hooks = []
                for hook in child:
                    hook_data = dict(hook.attrib)
                    if hook.text:
                        hook_data["content"] = hook.text.strip()
                    for hook_child in hook:
                        if hook_child.tag == "inputs":
                            inputs = {}
                            for inp in hook_child:
                                if inp.text:
                                    inputs[inp.get("name", inp.tag)] = inp.text.strip()
                            hook_data["inputs"] = inputs
                        elif hook_child.text:
                            hook_data[hook_child.tag] = hook_child.text.strip()
                    hooks.append(hook_data)
                result["hooks"] = hooks

            # Simple text fields - include empty strings for category
            elif tag == "category":
                result[tag] = (child.text or "").strip()
            elif child.text:
                result[tag] = child.text.strip()

        # Also check for description inside metadata if not found at root
        if "description" not in result:
            meta_desc = metadata_elem.find("description")
            if meta_desc is not None and meta_desc.text:
                result["description"] = meta_desc.text.strip()

    # Extract inputs
    inputs_elem = root.find("inputs")
    if inputs_elem is not None:
        inputs = []
        for inp in inputs_elem.findall("input"):
            input_data = {
                "name": inp.get("name", ""),
                "type": inp.get("type", "string"),
                "required": inp.get("required", "false").lower() == "true",
            }
            if inp.get("default") is not None:
                input_data["default"] = inp.get("default")
            if inp.text:
                input_data["description"] = inp.text.strip()
            inputs.append(input_data)
        if inputs:
            result["inputs"] = inputs

    # Extract actions (execute/search/load/sign tool calls) from anywhere
    # in the directive tree, excluding metadata internals (where the same
    # tag names are used declaratively for permissions).
    _metadata_elems: set = set()
    if metadata_elem is not None:
        _metadata_elems.add(metadata_elem)
        for _m in metadata_elem.iter():
            _metadata_elems.add(_m)

    actions = []
    for elem in root.iter():
        if elem.tag not in PRIMARY_ACTIONS or elem in _metadata_elems:
            continue
        action = {"primary": elem.tag}
        action.update(elem.attrib)
        params = {}
        for param in elem.findall("param"):
            pname = param.get("name", "")
            pval = param.get("value", "")
            if not pval and param.text:
                pval = param.text.strip()
            if pname:
                params[pname] = pval
        if params:
            action["params"] = params
        actions.append(action)
    if actions:
        result["actions"] = actions

    # Extract outputs
    outputs_elem = root.find("outputs")
    if outputs_elem is not None:
        outputs = []
        for output in outputs_elem.findall("output"):
            output_data = {
                "name": output.get("name", ""),
            }
            if output.text:
                output_data["description"] = output.text.strip()
            outputs.append(output_data)
        if outputs:
            result["outputs"] = outputs
