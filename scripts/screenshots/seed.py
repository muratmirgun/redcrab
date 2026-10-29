#!/usr/bin/env python3
"""The README screenshots' data: a small shop's users, sessions, products,
carts, leaderboard, job queue and order stream, written as Redis protocol
for `redis-cli --pipe`.

    seed.py data        # the keys
    seed.py load N      # N commands of traffic, mostly reads, for the charts
"""

import json
import random
import sys

FIRST = """Ada Alan Grace Linus Margaret Ken Barbara Dennis Frances Edsger Radia
Donald Sophie Guido Yukihiro Anders Bjarne Hedy Katherine Tim Shafi Leslie
Niklaus John Joan Ivan Lynn Chris Mary Rasmus""".split()
LAST = """Lovelace Turing Hopper Torvalds Hamilton Thompson Liskov Ritchie Allen
Dijkstra Perlman Knuth Wilson Rossum Matsumoto Hejlsberg Stroustrup Lamarr
Johnson Berners-Lee Goldwasser Lamport Wirth Backus Clarke Sutherland Conway
Lattner Keller Lerdorf""".split()
COUNTRIES = ["TR", "DE", "US", "GB", "NL", "FR", "JP", "BR", "CA", "SE"]
PLANS = ["free", "free", "free", "pro", "pro", "team"]
PRODUCTS = [
    ("kb-75", "Mechanical keyboard, 75%", "keyboards", 129.0),
    ("kb-tkl", "Tenkeyless keyboard", "keyboards", 99.0),
    ("ms-ergo", "Vertical ergonomic mouse", "mice", 59.0),
    ("ms-trackball", "Thumb trackball", "mice", 79.0),
    ("mon-27-4k", "27\" 4K monitor", "monitors", 389.0),
    ("mon-34-uw", "34\" ultrawide monitor", "monitors", 549.0),
    ("hp-anc", "Noise cancelling headphones", "audio", 249.0),
    ("mic-usb", "USB condenser microphone", "audio", 119.0),
    ("cam-4k", "4K webcam", "video", 159.0),
    ("dock-tb4", "Thunderbolt 4 dock", "accessories", 229.0),
    ("stand-arm", "Monitor arm", "accessories", 89.0),
    ("mat-xl", "Desk mat, XL", "accessories", 29.0),
]
EVENTS = ["created", "paid", "packed", "shipped", "delivered"]

out = sys.stdout.buffer


def cmd(*args):
    parts = [b"*%d\r\n" % len(args)]
    for arg in args:
        data = arg if isinstance(arg, bytes) else str(arg).encode()
        parts.append(b"$%d\r\n%s\r\n" % (len(data), data))
    out.write(b"".join(parts))


def data():
    random.seed(7)
    users = range(1000, 2200)
    for uid in users:
        first, last = random.choice(FIRST), random.choice(LAST)
        cmd(
            "HSET", f"user:{uid}",
            "name", f"{first} {last}",
            "email", f"{first.lower()}.{last.lower()}@example.com",
            "plan", random.choice(PLANS),
            "country", random.choice(COUNTRIES),
            "created_at", f"2026-{random.randint(1, 10):02}-{random.randint(1, 28):02}",
            "logins", random.randint(1, 400),
        )
    # A user to show: every field filled.
    cmd(
        "HSET", "user:1042",
        "name", "Ada Lovelace", "email", "ada@example.com", "plan", "team",
        "country", "GB", "created_at", "2026-03-14", "logins", 1815,
        "last_seen", "2026-10-29T08:41:07Z", "newsletter", "true",
    )
    for _ in range(400):
        token = "%016x" % random.getrandbits(64)
        session = {
            "user": random.choice(users),
            "ip": f"10.{random.randint(0, 255)}.{random.randint(0, 255)}.{random.randint(1, 254)}",
            "agent": random.choice(["Safari/18", "Firefox/131", "Chrome/130", "redcrab-tui"]),
            "cart": random.randint(0, 5),
        }
        cmd("SET", f"session:{token}", json.dumps(session), "EX", random.randint(600, 86400))
    for sku, name, category, price in PRODUCTS:
        product = {
            "sku": sku,
            "name": name,
            "category": category,
            "price": price,
            "currency": "EUR",
            "stock": random.randint(0, 250),
            "rating": round(random.uniform(3.6, 4.9), 1),
            "tags": random.sample(["new", "bestseller", "sale", "wireless", "usb-c", "eco"], 2),
            "dimensions": {"w": random.randint(10, 80), "h": random.randint(2, 50), "unit": "cm"},
        }
        cmd("JSON.SET", f"product:{sku}", "$", json.dumps(product))
    for uid in random.sample(list(users), 150):
        args = ["HSET", f"cart:{uid}"]
        for sku, *_ in random.sample(PRODUCTS, random.randint(1, 4)):
            args += [sku, random.randint(1, 3)]
        cmd(*args)
        cmd("EXPIRE", f"cart:{uid}", random.randint(3600, 7 * 86400))
    for uid in random.sample(list(users), 250):
        cmd("ZADD", "leaderboard:weekly", random.randint(100, 98000), f"user:{uid}")
    for n in range(80):
        job = {"to": f"user:{random.choice(users)}", "template": random.choice(
            ["welcome", "order-shipped", "password-reset", "weekly-digest"]), "attempt": 1}
        cmd("RPUSH", "queue:emails", json.dumps(job))
    for n in range(500):
        cmd(
            "XADD", "stream:orders", "*",
            "order", 50000 + n, "user", f"user:{random.choice(users)}",
            "event", random.choice(EVENTS), "total", f"{random.uniform(19, 900):.2f}",
        )
    cmd("SADD", "tags:trending", "mechanical", "ultrawide", "usb-c", "ergonomic", "4k", "anc")
    cmd(
        "HSET", "config:features",
        "new_checkout", "on", "dark_mode", "on", "recommendations", "beta",
        "gift_cards", "off", "max_cart_items", 50,
    )
    for _ in range(200):
        ip = f"203.0.113.{random.randint(1, 254)}"
        # Long enough to outlive the screenshots, so every one counts them.
        cmd("SET", f"rate:{ip}", random.randint(1, 120), "EX", 900)
    page = "<section class=\"product-grid\">" + "".join(
        f"<article data-sku=\"{sku}\"><h2>{name}</h2><p>{price:.2f} EUR</p></article>"
        for sku, name, _, price in PRODUCTS
    ) + "</section>"
    for path in ["/", "/keyboards", "/mice", "/monitors", "/audio", "/deals"]:
        cmd("SET", f"cache:page:{path}", page * random.randint(20, 120), "EX", 3600)
    for day in range(1, 29):
        cmd("PFADD", f"stats:visitors:2026-10-{day:02}", *[f"v{random.randint(0, 9000)}" for _ in range(60)])


def load(count):
    random.seed()
    users = range(1000, 2200)
    for _ in range(count):
        roll = random.random()
        if roll < 0.4:
            cmd("HGET", f"user:{random.choice(users)}", "name")
        elif roll < 0.6:
            cmd("JSON.GET", f"product:{random.choice(PRODUCTS)[0]}", "$.price")
        elif roll < 0.75:
            cmd("ZREVRANGE", "leaderboard:weekly", 0, 9, "WITHSCORES")
        elif roll < 0.85:
            # Sessions come and go: some miss.
            cmd("GET", "session:%016x" % random.getrandbits(64))
        elif roll < 0.95:
            cmd("HINCRBY", f"user:{random.choice(users)}", "logins", 1)
        else:
            cmd("ZINCRBY", "leaderboard:weekly", random.randint(1, 50), f"user:{random.choice(users)}")


if __name__ == "__main__":
    if sys.argv[1:2] == ["data"]:
        data()
    elif sys.argv[1:2] == ["load"]:
        load(int(sys.argv[2]))
    else:
        sys.exit(__doc__)
