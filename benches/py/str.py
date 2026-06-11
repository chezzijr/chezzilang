# Bench: f-string + join, 500k parts.

def main():
    parts = []
    i = 0
    while i < 500000:
        parts.append(f"item-{i}")
        i += 1
    s = ",".join(parts)
    print(len(s))

main()
