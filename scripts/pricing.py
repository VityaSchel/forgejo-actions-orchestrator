#!/usr/bin/env python3

from __future__ import annotations

import json
import os
import re
import sys
import urllib.error
import urllib.request
from concurrent.futures import ThreadPoolExecutor
from dataclasses import dataclass, field
from datetime import date
from pathlib import Path

CONFIG = Path(__file__).resolve().parent.parent / "config.example.toml"
MARKER = "#! Provider prices (last updated"
AS_OF = re.compile(r"as of .+ at the end of the file")
VENDOR_SUFFIX = re.compile(r"-(amd|intel)$")
VENDOR_IN_DESCRIPTION = re.compile(r" (AMD|Intel) ")

HETZNER_FAMILIES = ["cx", "cax", "cpx", "ccx"]
VULTR_TYPES = ["vc2", "vhp", "vhf"]
CHERRY_TYPES = ["vps", "vds", "premium-vds", "performance-vds", "arm-vds"]
SCALEWAY_FAMILIES = ["BASIC3"]
SCALEWAY_ZONES = [
	"fr-par-1", "fr-par-2", "fr-par-3",
	"nl-ams-1", "nl-ams-2", "nl-ams-3",
	"pl-waw-1", "pl-waw-2", "pl-waw-3",
]
GCORE_FAMILIES = ["g1-standard", "g2a-standard", "a1-standard"]
MAX_SPLIT_TIERS = 4
NAMED = 4
SYMBOLS = {"EUR": "€", "USD": "$"}

FAMILY_WORDING = {
	"hetzner/cx": {
		"cpu": "shared vCPU Intel/AMD x86",
		"note": "Cost-optimized. Limited availability.",
	},
	"hetzner/cax": {
		"cpu": "shared vCPU Ampere Arm64",
		"note": "Cost-optimized. Limited availability.",
	},
	"hetzner/cpx": {"cpu": "shared vCPU AMD x86", "note": "Regular performance."},
	"hetzner/ccx": {"cpu": "dedicated vCPU AMD x86", "note": "General purpose."},
	"scaleway/BASIC3 x86_64": {
		"cpu": "shared vCPU AMD x86",
		"note": "AMD EPYC 9555P (3.2 GHz).",
	},
	"scaleway/BASIC3 arm64": {
		"cpu": "shared vCPU Ampere Arm64",
		"note": "Ampere Altra Max M128-30.",
	},
}


@dataclass
class Plan:
	family: str
	name: str
	cpu_count: int
	ram: float
	cpu: str
	prices: dict[str, float] = field(default_factory=dict)


@dataclass
class Section:
	name: str
	currency: str
	plans: list[Plan]
	error: str | None = None


class Skipped(Exception):
	pass


def get(url, auth=None):
	headers = {"authorization": auth} if auth else {}
	request = urllib.request.Request(url, headers=headers)
	try:
		with urllib.request.urlopen(request, timeout=60) as response:
			return json.load(response)
	except urllib.error.HTTPError as error:
		detail = " ".join(error.read().decode(errors="replace").split())[:200]
		raise RuntimeError(f"{url} -> {error.code} {detail}") from None


def need(variable):
	value = os.environ.get(variable)
	if not value:
		raise Skipped(f"{variable} is not set")
	return value


def family_prefix(name):
	match = re.match(r"[a-zA-Z]+", name)
	return match.group() if match else name


def family_order(family, listed):
	return listed.index(family) if family in listed else len(listed)


def merge_vendor_variants(plans):
	merged, suffixed = [], {}
	for plan in plans:
		base = VENDOR_SUFFIX.sub("", plan.name)
		if base == plan.name:
			merged.append(plan)
			continue
		twin = suffixed.get(base)
		if twin is None:
			suffixed[base] = plan
			merged.append(plan)
			continue
		sold_by_both = {
			location: price
			for location, price in twin.prices.items()
			if location in plan.prices
		}
		if not sold_by_both:
			merged.append(plan)
			continue
		twin.name = f"{base}-{{amd,intel}}"
		twin.cpu = VENDOR_IN_DESCRIPTION.sub(" ", twin.cpu)
		twin.prices = sold_by_both
	return merged


def hetzner():
	auth = f"Bearer {need('HETZNER_TOKEN')}"
	types, page = [], 1
	while page:
		body = get(
			f"https://api.hetzner.cloud/v1/server_types?page={page}&per_page=50", auth
		)
		types += body["server_types"]
		page = body.get("meta", {}).get("pagination", {}).get("next_page")
	plans = [
		Plan(
			family_prefix(kind["name"]),
			kind["name"],
			kind["cores"],
			kind["memory"],
			f"{kind['cpu_type']} vCPU {kind['architecture']}",
			{p["location"]: float(p["price_hourly"]["net"]) for p in kind["prices"]},
		)
		for kind in types
		if not kind["deprecated"]
	]
	plans.sort(key=lambda plan: family_order(plan.family, HETZNER_FAMILIES))
	return Section("Hetzner", "EUR", plans)


def vultr():
	raw, cursor = [], ""
	while cursor is not None:
		body = get(
			"https://api.vultr.com/v2/plans?per_page=500"
			+ (f"&cursor={cursor}" if cursor else "")
		)
		raw += body["plans"]
		cursor = body.get("meta", {}).get("links", {}).get("next") or None
	report("vultr", [plan["type"] for plan in raw], VULTR_TYPES)
	plans = [
		Plan(
			plan["type"],
			plan["id"],
			plan["vcpu_count"],
			plan["ram"] / 1024,
			f"{'shared' if plan['vcpu_type'] == 'thread' else 'dedicated'}"
			f" vCPU {plan['cpu_vendor']} x86",
			{
				location: (plan.get("location_cost") or {})
				.get(location, {})
				.get("hourly_cost", plan["hourly_cost"])
				for location in plan["locations"]
			},
		)
		for plan in raw
		if plan["type"] in VULTR_TYPES and plan["hourly_cost"] > 0
	]
	return Section("Vultr", "USD", merge_vendor_variants(plans))


def cherry():
	raw = get("https://api.cherryservers.com/v1/plans")
	report("cherry", [plan["type"] for plan in raw], CHERRY_TYPES)
	plans = []
	for plan in raw:
		if plan["type"] not in CHERRY_TYPES:
			continue
		hourly = next((p for p in plan["pricing"] if p["unit"] == "Hourly"), None)
		if not hourly:
			continue
		cpus = plan["specs"]["cpus"]
		model = re.match(r"\s*\d+\s*x\s*(.+)$", cpus.get("name") or "", re.I)
		dedicated = (plan.get("category") or "").startswith("Dedicated")
		plans.append(Plan(
			plan["type"],
			plan["slug"],
			cpus["count"] * cpus["threads"],
			plan["specs"]["memory"]["total"],
			f"{'dedicated' if dedicated else 'shared'} vCPU"
			+ (f" {model.group(1).strip()}" if model else ""),
			{region["slug"]: hourly["price"] for region in plan["available_regions"]},
		))
	return Section("Cherry", "EUR", plans)


def scaleway():
	plans, seen = {}, []
	for zone in SCALEWAY_ZONES:
		servers = get(
			f"https://api.scaleway.com/instance/v1/zones/{zone}/products/servers"
		)["servers"]
		for name, spec in servers.items():
			prefix = name.split("-")[0]
			seen.append(prefix)
			if spec["end_of_service"] or prefix not in SCALEWAY_FAMILIES:
				continue
			plan = plans.setdefault(name, Plan(
				f"{prefix} {spec['arch']}",
				name,
				spec["ncpus"],
				spec["ram"] / 1024 ** 3,
				f"shared vCPU {spec['arch']}",
			))
			plan.prices[zone] = spec["hourly_price"]
	report("scaleway", seen, SCALEWAY_FAMILIES)
	return Section("Scaleway", "EUR", list(plans.values()))


def gcore():
	auth = f"APIKey {need('GCORE_TOKEN')}"
	project = need("GCORE_PROJECT_ID")
	regions = get("https://api.gcore.com/cloud/v1/regions?limit=1000", auth)
	plans, seen, currency = {}, [], "USD"
	for region in regions["results"]:
		try:
			flavors = get(
				f"https://api.gcore.com/cloud/v1/flavors/{project}/{region['id']}"
				"?include_prices=true&exclude_windows=true&limit=1000",
				auth,
			)
		except RuntimeError as error:
			print(f"gcore: skipping region {region['id']}: {error}", file=sys.stderr)
			continue
		for flavor in flavors["results"]:
			name = flavor["flavor_name"]
			seen.append(re.sub(r"-\d+-\d+$", "", name))
			family = next((p for p in GCORE_FAMILIES if name.startswith(p)), None)
			if not family or flavor["disabled"] or flavor.get("price_per_hour") is None:
				continue
			currency = flavor.get("currency_code") or currency
			hardware = flavor.get("hardware_description") or {}
			plan = plans.setdefault(name, Plan(
				family,
				name,
				flavor["vcpus"],
				flavor["ram"] / 1024,
				f"vCPU {hardware.get('cpu') or flavor['architecture']}",
			))
			plan.prices[f"{region['id']} ({region['display_name']})"] = \
				flavor["price_per_hour"]
	report("gcore", seen, GCORE_FAMILIES)
	return Section("Gcore", currency, list(plans.values()))


def report(provider, seen, kept):
	dropped = sorted({name for name in seen if name not in kept})
	if dropped:
		print(f"{provider}: not listed: {', '.join(dropped)}", file=sys.stderr)


def tiers(plans):
	def plans_sold_in(location):
		return [plan for plan in plans if location in plan.prices]

	def prices_agree(table, prices):
		shared = [name for name in prices if name in table]
		return bool(shared) and all(table[name] == prices[name] for name in shared)

	def tier(locations, table, names):
		rows = [(plan, table[plan.name]) for plan in plans if plan.name in names]
		return locations, rows

	locations = sorted(
		dict.fromkeys(location for plan in plans for location in plan.prices),
		key=lambda location: -len(plans_sold_in(location)),
	)
	price_tables = []
	for location in locations:
		prices = {
			plan.name: plan.prices[location] for plan in plans_sold_in(location)
		}
		for tier_locations, table in price_tables:
			if prices_agree(table, prices):
				tier_locations.append(location)
				table.update(prices)
				break
		else:
			price_tables.append(([location], prices))
	availability_split = []
	for tier_locations, table in price_tables:
		names_by_availability = {}
		for plan in plans:
			if plan.name not in table:
				continue
			sold_in = tuple(
				location for location in tier_locations if location in plan.prices
			)
			names_by_availability.setdefault(sold_in, []).append(plan.name)
		availability_split += [
			tier(list(sold_in), table, names)
			for sold_in, names in names_by_availability.items()
		]
	if len(availability_split) <= MAX_SPLIT_TIERS:
		return availability_split
	return [
		tier(tier_locations, table, list(table))
		for tier_locations, table in price_tables
	]


def money(value, currency):
	trimmed = f"{value:.5f}".rstrip("0")
	decimals = len(trimmed) - trimmed.index(".") - 1
	return SYMBOLS.get(currency, f"{currency} ") + (
		f"{value:.3f}" if decimals < 3 else trimmed
	)


def amount(value):
	return str(int(value)) if float(value).is_integer() else f"{value:.1f}"


def wrap(head, items, width=76):
	indent = "#!" + " " * (len(head) - 2)
	lines = [head]
	for item in items:
		if len(lines[-1]) + len(item) + 2 > width and lines[-1] != head:
			lines.append(f"{indent} {item},")
		else:
			lines[-1] = f"{lines[-1]} {item},"
	lines[-1] = lines[-1].rstrip(",")
	return lines


def banner(name, width=57):
	bar = "=" * ((width - len(name) - 2) // 2)
	return f"#! {bar} {name} {bar}"


def availability_note(plan, locations):
	missing = [where for where in locations if where not in plan.prices]
	sold = [where for where in locations if where in plan.prices]
	if not missing:
		return ""
	if len(missing) <= NAMED:
		return f" (not in {', '.join(missing)})"
	if len(sold) <= NAMED:
		return f" (only in {', '.join(sold)})"
	return f" (in {len(sold)} of {len(locations)} locations)"


def render(section):
	out = ["", banner(section.name), ""]
	if section.error:
		return out + [f"#! Not generated: {section.error}"]
	if not section.plans:
		return out + ["#! No plans matched."]
	for family in dict.fromkeys(plan.family for plan in section.plans):
		wording = FAMILY_WORDING.get(f"{section.name.lower()}/{family}", {})
		plans = sorted(
			(plan for plan in section.plans if plan.family == family),
			key=lambda plan: (plan.cpu_count, plan.ram),
		)
		name_width = max(len(plan.name) for plan in plans)
		count_width = max(len(str(plan.cpu_count)) for plan in plans)
		ram_width = max(len(amount(plan.ram)) for plan in plans)
		cpu_width = max(len(wording.get("cpu") or plan.cpu) for plan in plans) + 1
		if wording.get("note"):
			out.append(f"#! {wording['note']}")
		for locations, rows in tiers(plans):
			out += wrap(f"#! {family} |", locations)
			for plan, price in rows:
				cpu = f"{wording.get('cpu') or plan.cpu},"
				out.append(
					f"#! {plan.name:>{name_width}}: {plan.cpu_count:>{count_width}}"
					f" {cpu:<{cpu_width}} {amount(plan.ram):>{ram_width}} GB RAM,"
					f" {money(price, section.currency)}/hr"
					+ availability_note(plan, locations)
				)
			out.append("")
	return out[:-1]


def collect(source):
	try:
		return source()
	except (Skipped, RuntimeError, OSError, ValueError) as error:
		return Section(source.__name__.capitalize(), "", [], str(error))


SOURCES = [hetzner, vultr, cherry, scaleway, gcore]

today = date.today().isoformat()
with ThreadPoolExecutor(len(SOURCES)) as pool:
	sections = list(pool.map(collect, SOURCES))

generated_by = "#! Generated by scripts/pricing.py, do not edit by hand."
excludes_vat = "#! Prices exclude VAT."
block = "\n".join(
	[f"{MARKER} {today})", generated_by, excludes_vat]
	+ [line for section in sections for line in render(section)]
	+ [""]
)

if "--write" not in sys.argv:
	print(block)
	raise SystemExit

skipped = [section.error for section in sections if section.error]
if skipped:
	sys.exit(f"refusing to write: {'; '.join(skipped)}")

config = CONFIG.read_text()
start = config.find(MARKER)
if start < 0:
	sys.exit(f'no "{MARKER}" line in config.example.toml')
CONFIG.write_text(
	AS_OF.sub(f"as of {today} at the end of the file", config[:start]) + block
)
print(f"wrote {CONFIG}", file=sys.stderr)
