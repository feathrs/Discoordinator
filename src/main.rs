extern crate crossbeam;
extern crate fixed_vec_deque;
extern crate cmd;
extern crate lru;
extern crate parking_lot;
extern crate serenity;
extern crate bimap;

use crossbeam::scope;
use fixed_vec_deque::FixedVecDeque;
use lru::LruCache;
use parking_lot::{RwLock, RwLockWriteGuard};
use serenity::http::Http;
use serenity::model::prelude::*;
use serenity::prelude::*;
use std::collections::{BTreeMap, HashMap, BTreeSet};
use std::sync::Arc;
use std::thread::sleep;
use std::time::{Duration, Instant};
use cmd::Args;
use bimap::BiBTreeMap;

type CategoryCache = LruCache<ChannelId, (ChannelId, Option<ChannelId>)>;
type CleanupQueue = FixedVecDeque<[(ChannelId, ChannelId, Option<ChannelId>); 32]>;

const PARTY_PREFIX: &str = "+# ";

static mut USER_ID: UserId = UserId(0);

fn user_id() -> UserId {
    unsafe {USER_ID} // I solemnly swear that I am up to no good
}

struct Bot {
    perms_member: Permissions,
    perms_creator: Permissions,
    cleanup_queue: RwLock<CleanupQueue>,
    voice_counts: RwLock<BTreeMap<ChannelId, u8>>,
    voice_channels: RwLock<BTreeMap<UserId, ChannelId>>,
    category_cache: RwLock<CategoryCache>,
    // category cache is actually vc -> category + txt
    ignore_cache: RwLock<LruCache<ChannelId, ()>>,
    owner_cache: RwLock<BiBTreeMap<ChannelId, (UserId, GuildId)>>,
    // I'm assuming that ChannelId has implied independent domain to GuildId.
    move_role_cache: RwLock<BTreeSet<RoleId>>, // to identify if user has perms to move user
    create_chan_role_cache: RwLock<BTreeSet<RoleId>>, // to identify if user has perms to create channel
    // god forbid should two servers have two roles with identical ids
    guild_owner_cache: RwLock<BTreeMap<GuildId, UserId>>, // Owner always has Administrator perms
    whitelist_role_cache: RwLock<BTreeMap<GuildId, RoleId>>,
    ratelimit_cache: RwLock<LruCache<UserId, Instant>>
    // May use (UserId, GuildId) keying instead if people find there is a legitimate need to create
    // multiple parties across guilds within the ratelimit.
}

impl Bot {
    fn update_guild_cache(
        &self,
        ctx: &Context,
        guild: GuildId,
        cache_handle: &mut RwLockWriteGuard<CategoryCache>,
    ) {
        let channel_info = guild.channels(&ctx);
        if channel_info.is_err() {
            // This is a disaster!
            eprintln!("Failed to get channels for {}; {:?}", guild.0, channel_info);
            return;
        }
        let channel_info = channel_info.unwrap();
        let mut category_map = BTreeMap::new();
        let mut category_list = Vec::new();
        for (id, info) in channel_info {
            match info.kind {
                ChannelType::Category => {
                    if info.name.starts_with(PARTY_PREFIX) {
                        category_list.push(id);
                    }
                }
                ChannelType::Text | ChannelType::Voice => {
                    if let Some(cat_id) = info.category_id {
                        let mut entry = category_map.entry(cat_id).or_insert((ChannelId(0), None));
                        if info.kind == ChannelType::Text {
                            entry.1 = Some(id);
                        } else {
                            entry.0 = id;
                        }
                    }
                }
                _ => {}
            }
        }
        for cat_id in category_list {
            if let Some((vc_id, txt_id)) = category_map.remove(&cat_id) {
                cache_handle.put(vc_id, (cat_id, txt_id));
            }
        }
    }

    fn update_role(&self, role: &Role) {
        Self::update_role_raw(&mut self.move_role_cache.write(), &mut self.create_chan_role_cache.write(), role);
    }

    fn update_role_raw(move_role_cache: &mut RwLockWriteGuard<BTreeSet<RoleId>>, create_chan_role_cache: &mut RwLockWriteGuard<BTreeSet<RoleId>>, role: &Role) {
        if role.permissions.move_members() || role.permissions.administrator() {
            move_role_cache.insert(role.id);
        } else {
            move_role_cache.remove(&role.id);
        }
        if role.permissions.manage_channels() || role.permissions.administrator() {
            create_chan_role_cache.insert(role.id);
        } else {
            create_chan_role_cache.remove(&role.id);
        }
    }
}

impl EventHandler for Bot {
    fn message(&self, ctx: Context, message: Message) {
        // First, ignore messages by bots.
        if message.author.bot || message.guild_id.is_none() {
            return;
        }
        let guild = message.guild_id.unwrap();
        if message.content.starts_with("/party") {
            let now = Instant::now();
            let since = self.ratelimit_cache.read().peek(&message.author.id)
                .map(|&last| now.duration_since(last))
                .unwrap_or_else(|| Duration::from_secs(301));
            if since < Duration::from_secs(20) {
                // This is both for the bot's sake and to prevent nuisance abuse of the bot
                return;
            } else if self.owner_cache.read().contains_right(&(message.author.id, guild)) {
                let _ = message.reply(&ctx, "You already have a party! Disband it first.");
                self.ratelimit_cache.write().put(message.author.id, now);
                return;
            } else if since < Duration::from_secs(300) {
                let _ = message.reply(&ctx, format!("You're making parties too fast! Wait another {} seconds", 300-since.as_secs()));
                return;
            }
            self.ratelimit_cache.write().put(message.author.id, Instant::now());
            if let Some(&role_id) = self.whitelist_role_cache.read().get(&guild) {
                let member = message.member.as_ref().unwrap();
                let chan_role_cache = self.create_chan_role_cache.read();
                if !(member.roles.iter().any(|r| *r == role_id || chan_role_cache.contains(r))
                    || self.guild_owner_cache.read().get(&guild) == Some(&message.author.id))  {
                    // This needs rate-limiting too or people will be extremely funny.
                    let _ = message.reply(&ctx, "You do not have permission to use this command");
                    return;
                }
            }
            let args = Args::parse(&message.content[6..]);
            if args.is_err() {
                let _ = message.reply(ctx, "Failed to parse command!");
                return;
            }
            let args = args.unwrap();
            let name_part = if let Some(name) = args.kwargs.get("name") {
                name.chars().take(20).collect::<String>()
            } else if let Some(name) = args.args.get(0) {
                if name.parse::<UserId>().is_ok() {
                    // It's actually a user, so just give it a default name
                    message.id.to_string()
                } else {
                    // It's not a user, so they probably intended to set the name
                    name.chars().take(20).collect::<String>()
                    // I'm not entirely sure why - As far as I'm aware - Rust doesn't provide a way
                    // to get the char at a byte offset, given that it can just walk back until
                    // is_char_boundary(i). Then I can just slice up to there and I don't have
                    // iterate, collect, and copy.
                }
            } else {
                message.id.to_string()
            };
            // Set up the initial permissions
            let listed_users = args
                .args
                .iter()
                .filter_map(|arg| arg.parse::<UserId>().ok());
            let users = listed_users.clone()
                .chain(std::iter::once(message.author.id))
                .chain(std::iter::once(user_id()));
            let initial_user_perms = users
                .clone()
                .map(|user| PermissionOverwrite {
                    allow: self.perms_creator,
                    deny: Permissions::empty(),
                    kind: PermissionOverwriteType::Member(user),
                })
                .chain(std::iter::once(PermissionOverwrite {
                    allow: Permissions::empty(),
                    deny: self.perms_member,
                    kind: PermissionOverwriteType::Role(RoleId(guild.0)),
                }));

            // Create a category
            let cat = guild.create_channel(&ctx, |c| {
                c.name(format!("{}{}", PARTY_PREFIX, &name_part))
                    .permissions(initial_user_perms.clone())
                    .kind(ChannelType::Category)
                    .position(200)
            });
            let cat = if let Ok(cat) = cat {cat} else {
                let _ = message.reply(&ctx, "Failed to create category.");
                return;
            };

            // Create the channels
            let vc = guild.create_channel(&ctx, |c| {
                c.name(format!("Party: {}", name_part))
                    .position(200)
                    //.permissions(initial_user_perms.clone())
                    .kind(ChannelType::Voice)
                    .category(cat.id)
            });
            let vc = if let Ok(vc) = vc {vc} else {
                let _ = message.reply(&ctx, "Failed to create VC.");
                let res = cat.delete(&ctx);
                if res.is_err() {
                    let _ = message.reply(&ctx, "Also failed to delete the category. Disaster.");
                }
                return;
            };
            self.owner_cache.write().insert(vc.id, (message.author.id, guild));

            let txt = guild
                .create_channel(&ctx, |c| {
                    c.name(format!("party-{}", name_part))
                        .position(200)
                        .kind(ChannelType::Text)
                        //.permissions(initial_user_perms)
                        .category(cat.id)
                })
                .ok();
            if txt.is_none() {
                let _ = message.reply(&ctx, "Failed to create text channel. Voice only.");
            }

            // Add that shit to the cache.
            let mut cat_cache = self.category_cache.write();
            cat_cache.put(vc.id, (cat.id, txt.as_ref().map(|c| c.id)));

            // Now, if the user is in voice, we should move them.
            let moved = guild.move_member(&ctx, message.author.id, vc.id);
            // If we can't move them, schedule the channel to be checked again
            // after a couple of minutes and to be deleted if it is not in use.
            if moved.is_err() {
                let mut queue = self.cleanup_queue.write();
                if queue.is_full() {
                    let old = queue.front().unwrap();
                    // We're about to write over the last so we should check it
                    // If it's empty, tidy it
                    let count = self.voice_counts.read().get(&old.0).copied().unwrap_or(0);
                    if count == 0 {
                        let _ = old.1.delete(&ctx);
                        if let Some(ref txt) = old.2 {
                            let _ = txt.delete(&ctx);
                        }
                        let _ = old.1.delete(&ctx);
                        // This should work because the last use of the read lock
                        // was above. NLL or something good like that. If not I
                        // can manually drop(map) anyway.
                        self.voice_counts.write().remove(&old.0);
                        // Also clean the owner cache for the channel
                        self.owner_cache.write().remove_by_left(&old.1);
                    }
                    // If it's not empty, it'll get cleaned later.
                }
                *queue.push_back() = (cat.id, vc.id, txt.map(|c| c.id));
            } else {
                // If we moved them just fine, check if we should move everyone else they've added
                // The users iterator includes the owner but this should be a fine no-op.
                let role_cache = self.move_role_cache.read();
                if message.member.unwrap().roles.iter().any(|r| role_cache.contains(r))
                    || self.guild_owner_cache.read().get(&guild) == Some(&message.author.id)
                {
                    for user in listed_users.clone() {
                        // Dump the result, we don't actually care if they succeeded.
                        let _ = guild.move_member(&ctx, user, vc.id);
                    }
                }

            };

        }
    }

    fn voice_state_update(&self, ctx: Context, guild: Option<GuildId>, voice: VoiceState) {
        if guild.is_none() {
            return;
        }
        let guild = guild.expect("what the fuck");
        let mut member_map = self.voice_channels.write();
        let mut count_map = self.voice_counts.write();
        if let Some(old_channel) = member_map.remove(&voice.user_id) {
            if let Some(old_count) = count_map.get_mut(&old_channel) {
                *old_count -= 1;
                if *old_count == 0 {
                    count_map.remove(&old_channel);
                    // Channel is empty; clean it up.
                    // Check for it in the category cache
                    let mut cache = self.category_cache.write();
                    if cache.peek(&old_channel).is_none() {
                        // We need to get the channels which match, so we should
                        // fetch all channels and update the cache for a server.
                        self.update_guild_cache(&ctx, guild, &mut cache);
                    }
                    if let Some(chans) = cache.pop(&old_channel) {
                        let _ = old_channel.delete(&ctx);
                        if let Some(chan) = chans.1 {
                            let _ = chan.delete(&ctx);
                        }
                        let _ = chans.0.delete(&ctx);
                        self.owner_cache.write().remove_by_left(&old_channel);
                    } else {
                        eprintln!("Failed to get channels after cache reload for {:?}", guild);
                        // This could be an ignored channel: i.e. it's not managed by the bot
                        // If this keeps happening, look at updating the ignore
                        // cache at the same time. If it keeps happening then,
                        // look at dynamically scaling the cache when it happens.

                        // This is a hack.
                        println!("Ignoring {:?}", old_channel);
                        let mut ignore_cache = self.ignore_cache.write();
                        ignore_cache.put(old_channel, ());
                    }
                }
            } else {
                // We didn't actually have information on the channel.
                // It's game over really. There's nothing to be done here.
                eprintln!(
                    "A user disconnected from an uncached channel in {} ({})",
                    guild, old_channel
                );
            }
        }
        if let Some(chan) = voice.channel_id {
            let mut ignore_cache = self.ignore_cache.write();
            if ignore_cache.get(&chan).is_some() {
                // It's one of the ones we're already ignoring.
                return;
            }
            // Moved to a new channel
            member_map.insert(voice.user_id, chan);
            *count_map.entry(chan).or_insert(0) += 1;

            let owner_cache = self.owner_cache.read();
            if owner_cache.get_by_left(&chan) == Some(&(voice.user_id, guild)) { // .contains does not update LRU
                // The user is an owner of this channel. They already have perms.
                // Also I updated the way channel owners work so this is now slightly broken and
                // doesn't maintain the permissions for the initial users. I need to either fix that
                // or change the semantics of the initial user permissions.
                // TODO: Check and manage owners properly now the semantics of owner_cache has changed.
                return;
            }

            // If we're tracking it, we should make sure they have permissions.
            let mut cat_cache = self.category_cache.write();
            if let Some((cat_id, _)) = cat_cache.get(&chan) {
                let res = cat_id.create_permission(
                    &ctx,
                    &PermissionOverwrite {
                        allow: self.perms_member,
                        deny: Permissions::empty(),
                        kind: PermissionOverwriteType::Member(voice.user_id),
                    },
                );
                if res.is_err() {
                    eprintln!("Failed to set category perms; {:?}", res);
                }
            }
        }
    }

    fn ready(&self, ctx: Context, ready: Ready) {
        let guilds = ready.guilds.iter().filter_map(|status| 
            if let GuildStatus::OnlineGuild(guild) = status {Some(guild)} else {None}
        );
        let mut category_cache = self.category_cache.write();
        let mut voice_map = self.voice_channels.write(); // User channel tracker (for decrement)
        let mut counts = self.voice_counts.write(); // User channel counts
        let mut move_role_cache = self.move_role_cache.write();
        let mut create_chan_role_cache = self.create_chan_role_cache.write();
        let mut guild_owner_cache = self.guild_owner_cache.write();
        let mut whitelist_cache = self.whitelist_role_cache.write();
        for guild in guilds {
            // Update the role caches
            for (.., role) in &guild.roles {
                Self::update_role_raw(&mut move_role_cache, &mut create_chan_role_cache, role);
                if role.name.starts_with("+#") && whitelist_cache.insert(guild.id, role.id).is_some() {
                    eprintln!("{:?} has multiple '+#' roles", guild.id)
                    // In the event that they have multiple I'll need to sort out something smarter.
                    // I'm leaving this here has acknowledgement of that fact, giving me a way to
                    // defer implementing a smarter solution to a time that it is required.
                }
            }

            // Update guild-owner cache
            guild_owner_cache.insert(guild.id, guild.owner_id);

            // This code is copy-pasted
            // Please refactor.
            let mut category_map = HashMap::new();
            let mut category_list = Vec::new();
            for (&id, info) in &guild.channels {
                let info = info.read();
                match info.kind {
                    ChannelType::Category => {
                        if info.name.starts_with(PARTY_PREFIX) {
                            category_list.push(id);
                        }
                    }
                    ChannelType::Text | ChannelType::Voice => {
                        if let Some(cat_id) = info.category_id {
                            let mut entry = category_map.entry(cat_id).or_insert((ChannelId(0), None));
                            if info.kind == ChannelType::Text {
                                entry.1 = Some(id);
                            } else {
                                entry.0 = id;
                            }
                        }
                    }
                    _ => {}
                }
            }
            for cat_id in category_list {
                if let Some((vc_id, txt_id)) = category_map.remove(&cat_id) {
                    category_cache.put(vc_id, (cat_id, txt_id));
                }
            }
            
            // Populate the ignore cache with every channel not matched to a party
            let mut ignore = self.ignore_cache.write();
            for (_, (vc_id, _)) in category_map.drain() { 
                ignore.put(vc_id, ()); // I really need some kind of LRU set
            }

            for (&user, voice) in &guild.voice_states {
                *counts.entry(voice.channel_id.expect("User voice not in channel at ready")).or_insert(0) += 1;
                voice_map.insert(user, voice.channel_id.unwrap());
            }
        }

        for (&chan, &v) in counts.clone().iter() {
            println!("{}, {}", chan, v);
            let cat_info = category_cache.peek(&chan);
            if let Some(info) = cat_info {
                if v == 0 {
                    // Delete it
                    // category_cache is an LRU cache so removal will happen automatically
                    counts.remove(&chan);
                    let _ = chan.delete(&ctx);
                    if let Some(txt) = info.1 {
                        let _ = txt.delete(&ctx);
                    }
                    let _ = info.0.delete(&ctx);
                }
            }
        }

        unsafe {USER_ID = ready.user.id};
        //ctx.set_activity(/*activity*/);
        // Serenity doesn't support a custom activity
        // Despite this, it has the custom activity type
        // This is fucking stupid.
    }

    fn guild_role_create(&self, _ctx: Context, guild_id: GuildId, role: Role) {
        if role.permissions.move_members() || role.permissions.administrator() {
            self.move_role_cache.write().insert(role.id);
        }
        if role.name.starts_with("+#") {
            self.whitelist_role_cache.write().insert(guild_id, role.id);
        }
    }

    fn guild_role_delete(&self, _ctx: Context, guild_id: GuildId, role: RoleId) {
        self.move_role_cache.write().remove(&role);
        let mut whitelist_cache = self.whitelist_role_cache.write();
        if whitelist_cache.get(&guild_id) == Some(&role) {
            whitelist_cache.remove(&guild_id);
        }
    }

    fn guild_role_update(&self, _ctx: Context, guild_id: GuildId, role: Role) {
        self.update_role(&role);
        let mut whitelist_cache = self.whitelist_role_cache.write();
        if role.name.starts_with("+#") {
            whitelist_cache.insert(guild_id, role.id);
        } else if whitelist_cache.get(&guild_id) == Some(&role.id) {
            whitelist_cache.remove(&guild_id);
        }
    }

    fn guild_create(&self, _ctx: Context, guild: Guild) {
        let mut role_cache = self.move_role_cache.write();
        let mut chan_role_cache = self.create_chan_role_cache.write();
        for (.., role) in guild.roles {
            Self::update_role_raw(&mut role_cache, &mut chan_role_cache, &role);
        }
        self.guild_owner_cache.write().insert(guild.id, guild.owner_id);
    }
}

struct BotEventsDelegator(Arc<Bot>);

impl EventHandler for BotEventsDelegator {
    delegate::delegate! {
        to self.0 {
            fn message(&self, ctx: Context, message: Message);
            fn voice_state_update(&self, ctx: Context, guild: Option<GuildId>, voice: VoiceState);
            fn ready(&self, ctx: Context, _ready: Ready);
            fn guild_role_create(&self, ctx: Context, guild: GuildId, role: Role);
            fn guild_role_delete(&self, ctx: Context, guild: GuildId, role: RoleId);
            fn guild_role_update(&self, ctx: Context, guild: GuildId, role: Role);
            fn guild_create(&self, ctx: Context, guild: Guild);
        }
    }
}

fn main() {
    let perms_member: Permissions = Permissions::READ_MESSAGES
        | Permissions::SEND_MESSAGES
        | Permissions::CONNECT
        | Permissions::SPEAK
        | Permissions::MOVE_MEMBERS;
    let perms_creator: Permissions = perms_member
        | Permissions::MUTE_MEMBERS // p/ sure this won't apply to channels
        | Permissions::PRIORITY_SPEAKER
        | Permissions::MENTION_EVERYONE; // Only applies to a channel.

    let bot = Arc::new(Bot {
        perms_member,
        perms_creator,
        cleanup_queue: RwLock::new(FixedVecDeque::new()),
        voice_counts: Default::default(),
        voice_channels: Default::default(),
        category_cache: RwLock::new(CategoryCache::new(32)),
        ignore_cache: RwLock::new(LruCache::new(128)),
        owner_cache: Default::default(),
        ratelimit_cache: RwLock::new(LruCache::new(128)),
        move_role_cache: Default::default(),
        create_chan_role_cache: Default::default(),
        guild_owner_cache: Default::default(),
        whitelist_role_cache: Default::default(),
    });

    let mut token = std::env::args().nth(1).expect("No token supplied");
    if !token.starts_with("Bot ") {
        token = format!("Bot {}", token);
    }
    let http_client = Http::new_with_token(&token);
    scope(move |s| {
        println!("Preparing client");
        let mut client = Client::new(token, BotEventsDelegator(Arc::clone(&bot)))
            .expect("Failed to create client. Bad token?");
        println!("Client prepared");

        let guard = s.spawn(move |_| {
            let mut last = ChannelId(0);
            loop {
                println!("Checking for idle channels");
                sleep(Duration::from_secs(60));
                let mut cleanup = bot.cleanup_queue.write();
                let tail = cleanup.front();
                if let Some(tail) = tail {
                    println!("Checking {:?}", tail);
                    if tail.0 == last {
                        println!("Was the last checked.");
                        let tail = cleanup.pop_front().unwrap();
                        // It's safe, I promise. Probably.
                        let mut counts = bot.voice_counts.write();
                        if counts.get(&tail.1).copied().unwrap_or(0) == 0 {
                            println!("Nobody in the channel; Cleaning up.");
                            counts.remove(&tail.1);
                            if let Some(txt) = tail.2 {
                                let _ = txt.delete(&http_client);
                            }
                            let _ = tail.1.delete(&http_client);
                            let _ = tail.0.delete(&http_client);
                        }
                        // Clear the owner cache
                        bot.owner_cache.write().remove_by_left(&tail.1);
                    } else {
                        println!("Setting it as the last used.");
                        last = tail.0;
                    }
                }
            }
        });
        client.start().expect("Failed to start the bot.");
        guard.join().expect("Failed to join guard");
    })
    .expect("Failed to start crossbeam scope");
}
