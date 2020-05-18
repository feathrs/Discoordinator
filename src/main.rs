extern crate crossbeam;
extern crate fixed_vec_deque;
extern crate logos;
extern crate lru;
extern crate parking_lot;
extern crate serenity;

use crossbeam::scope;
use fixed_vec_deque::FixedVecDeque;
use lru::LruCache;
use parking_lot::{RwLock, RwLockWriteGuard};
use serenity::http::Http;
use serenity::model::prelude::*;
use serenity::prelude::*;
use std::collections::BTreeMap;
use std::sync::Arc;
use std::thread::sleep;
use std::time::Duration;

mod command;

type CategoryCache = LruCache<ChannelId, (ChannelId, Option<ChannelId>)>;

struct Bot {
    perms_member: Permissions,
    perms_creator: Permissions,
    cleanup_queue: RwLock<FixedVecDeque<[(ChannelId, ChannelId, Option<ChannelId>); 32]>>,
    voice_counts: RwLock<BTreeMap<ChannelId, u8>>,
    voice_channels: RwLock<BTreeMap<UserId, ChannelId>>,
    category_cache: RwLock<CategoryCache>,
    // category cache is actually vc -> category + txt
    ignore_cache: RwLock<LruCache<ChannelId, ()>>,
    owner_cache: RwLock<LruCache<(UserId, ChannelId), ()>>, // owner cache is just me being lazy about party owner checks
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
                    if info.name.starts_with("CX~") {
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
}

impl EventHandler for Bot {
    fn message(&self, ctx: Context, message: Message) {
        // First, ignore messages by bots.
        if message.author.bot || message.guild_id.is_none() {
            return;
        }
        let guild = message.guild_id.unwrap();
        if message.content.starts_with("/party") {
            let args = command::Args::parse(&message.content[6..]);
            if args.is_err() {
                let _ = message.reply(ctx, "Failed to parse command!");
                return;
            }
            let args = args.unwrap();
            let name = if let Some(name) = args.kwargs.get("name") {
                format!("CX~{}", name)
            } else {
                format!("CX~{}", message.id)
            };
            // Set up the initial permissions
            let users = args
                .args
                .iter()
                .filter_map(|arg| arg.parse::<UserId>().ok())
                .chain(std::iter::once(message.author.id));
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
                c.name(name)
                    .permissions(initial_user_perms.clone())
                    .kind(ChannelType::Category)
                    .position(200)
            });
            let cat = if cat.is_err() {
                let _ = message.reply(&ctx, "Failed to create category.");
                return;
            } else {
                cat.unwrap()
            };

            // Create the channels
            let vc = guild.create_channel(&ctx, |c| {
                c.name(format!("CX#{}", cat.id.0))
                    .position(200)
                    //.permissions(initial_user_perms.clone())
                    .kind(ChannelType::Voice)
                    .category(cat.id)
            });
            let vc = if vc.is_err() {
                let _ = message.reply(&ctx, "Failed to create VC.");
                let res = cat.delete(&ctx);
                if res.is_err() {
                    let _ = message.reply(&ctx, "Also failed to delete the category. Disaster.");
                }
                return;
            } else {
                vc.unwrap()
            };
            let mut owner_cache = self.owner_cache.write();
            for user in users {
                owner_cache.put((user, vc.id), ());
            }

            let txt = guild
                .create_channel(&ctx, |c| {
                    c.name(format!("cx-{}", cat.id.0))
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
                        let _ = vc.delete(&ctx);
                        if let Some(ref txt) = txt {
                            let _ = txt.delete(&ctx);
                        }
                        let _ = cat.delete(&ctx);
                        // This should work because the last use of the read lock
                        // was above. NLL or something good like that. If not I
                        // can manually drop(map) anyway.
                        self.voice_counts.write().remove(&old.0);
                    }
                    // If it's not empty, it'll get cleaned later.
                }
                *queue.push_back() = (cat.id, vc.id, txt.map(|c| c.id));
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
                    } else {
                        eprintln!("Failed to get channels after cache reload for {:?}", guild);
                        // This could be an ignored channel
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

            let mut owner_cache = self.owner_cache.write();
            if owner_cache.get(&(voice.user_id, chan)).is_some() {
                // The user is an owner of this channel. They already have perms
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

    fn ready(&self, ctx: Context, _: Ready) {
        //ctx.set_activity(/*activity*/);
        // Serenity doesn't support a custom activity
        // Despite this, it has the custom activity type
        // This is fucking stupid.
    }
}

struct BotEventsDelegator(Arc<Bot>);

impl EventHandler for BotEventsDelegator {
    delegate::delegate! {
        to self.0 {
            fn message(&self, ctx: Context, message: Message);
            fn voice_state_update(&self, ctx: Context, guild: Option<GuildId>, voice: VoiceState);
            fn ready(&self, ctx: Context, _ready: Ready);
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
        voice_counts: RwLock::new(BTreeMap::new()),
        voice_channels: RwLock::new(BTreeMap::new()),
        category_cache: RwLock::new(CategoryCache::new(32)),
        ignore_cache: RwLock::new(LruCache::new(128)),
        owner_cache: RwLock::new(LruCache::new(128)),
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
