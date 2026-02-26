# rye:signed:2026-02-26T05:52:24Z:9c042b21d10c761c08d208dd61a72233052e56c7151dac60f30590e3a3d734c2:ehBbzQBVggd07GC9j22_deDtn6qV6ut3XFpgBC1Ku8W8dk27LIEG5e5oMMmQXhMelWRz5eA2RqrlYx964GYHCQ==:4b987fd4e40303ac
"""Markdown XML parser for directives.

Handles extraction of XML from markdown code fences and parsing
with support for opaque sections (template, example, raw tags).
"""

__version__ = "1.1.0"
__tool_type__ = "parser"
__category__ = "rye/core/parsers/markdown"
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
            # Extract extends
            extends_match = re.search(r'extends\s*=\s*["\']([^"\']+)["\']', attrs_str)
            if extends_match:
                result["extends"] = extends_match.group(1)

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


def _coerce_value(s: str) -> Any:
    """Coerce a string value from XML to the appropriate Python type."""
    if s.lower() in ("true", "false"):
        return s.lower() == "true"
    try:
        return int(s)
    except ValueError:
        pass
    try:
        return float(s)
    except ValueError:
        pass
    return s


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
                    _ALLOWED_PERM_TAGS = PRIMARY_ACTIONS | {"acknowledge"}
                    for perm in child:
                        if perm.tag not in _ALLOWED_PERM_TAGS:
                            raise ValueError(
                                f"Unknown tag <{perm.tag}> inside <permissions>. "
                                f"Valid tags: {', '.join(sorted(_ALLOWED_PERM_TAGS))}"
                            )
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
                acknowledgments = []
                for ack in child.findall("acknowledge"):
                    risk = ack.get("risk", "")
                    reason = (ack.text or "").strip()
                    if risk:
                        acknowledgments.append({"risk": risk, "reason": reason})
                if acknowledgments:
                    result["acknowledged_risks"] = acknowledgments
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
                    if hook.text and hook.text.strip():
                        hook_data["content"] = hook.text.strip()
                    for hook_child in hook:
                        if hook_child.tag == "action":
                            # Check for nested primary actions (<execute>, <load>, etc.)
                            sub_actions = []
                            for sub in hook_child:
                                if sub.tag in PRIMARY_ACTIONS:
                                    sa = dict(sub.attrib)
                                    sa["primary"] = sub.tag
                                    sub_params = {}
                                    for sp in sub:
                                        if sp.tag == "param" and sp.text:
                                            sub_params[sp.get("name", sp.tag)] = sp.text.strip()
                                    if sub_params:
                                        sa["params"] = sub_params
                                    sub_actions.append(sa)
                            if sub_actions:
                                hook_data["actions"] = sub_actions
                            else:
                                # Single action with attributes on <action> itself
                                action = dict(hook_child.attrib)
                                params = {}
                                for param in hook_child:
                                    if param.tag == "param" and param.text:
                                        params[param.get("name", param.tag)] = param.text.strip()
                                if params:
                                    action["params"] = params
                                hook_data["action"] = action
                        elif hook_child.tag == "condition":
                            cond = dict(hook_child.attrib)
                            if "value" in cond:
                                cond["value"] = _coerce_value(cond["value"])
                            hook_data["condition"] = cond
                        elif hook_child.tag == "inputs":
                            inputs = {}
                            for inp in hook_child:
                                if inp.text:
                                    inputs[inp.get("name", inp.tag)] = inp.text.strip()
                            hook_data["inputs"] = inputs
                        elif hook_child.text:
                            hook_data[hook_child.tag] = hook_child.text.strip()
                    hooks.append(hook_data)
                result["hooks"] = hooks

            elif tag == "context":
                context = {"system": [], "before": [], "after": [], "suppress": []}
                for ctx_child in child:
                    position = ctx_child.tag
                    if position == "suppress" and ctx_child.text and ctx_child.text.strip():
                        context["suppress"].append(ctx_child.text.strip())
                    elif position in ("system", "before", "after") and ctx_child.text and ctx_child.text.strip():
                        context[position].append(ctx_child.text.strip())
                result["context"] = context

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
                "type": output.get("type", "string"),
                "required": output.get("required", "false").lower() == "true",
            }
            if output.text:
                output_data["description"] = output.text.strip()
            outputs.append(output_data)
        if outputs:
            result["outputs"] = outputs
