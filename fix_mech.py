with open('src/mechanics/mod.rs', 'r') as f:
    content = f.read()
content = content.replace('pub mod loyalty_checkin;', '')
with open('src/mechanics/mod.rs', 'w') as f:
    f.write(content)
print('Removed loyalty_checkin reference')
