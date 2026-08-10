with open('src/mechanics/mod.rs', 'r') as f:
    content = f.read()
# Add loyalty_checkin back
content = content.replace('pub mod scoring;', 'pub mod scoring;\npub mod loyalty_checkin;')
with open('src/mechanics/mod.rs', 'w') as f:
    f.write(content)
print('Added loyalty_checkin back')
