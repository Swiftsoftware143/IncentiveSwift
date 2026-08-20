-- Full lifecycle email template seed for IncentiveSwift (master-layout style).
-- All default (aid NULL, is_default=true). Per-account overrides use aid=<account_id>.
-- Dynamic vars: {{first_name}} {{last_name}} {{email}} {{campaign_name}} {{campaign_type}}
--   {{user_score}} {{reward_code}} {{prize_name}} {{expiry_date}} {{tier_name}} {{next_tier}}
--   {{ticket_number}} {{referral_link}} {{outcome}} {{entry_id}} {{share_link}} {{redeem_link}}

-- ================= QUIZ =================
INSERT INTO email_templates (template_type, name, subject, body, html_body, is_default, aid) VALUES
('entry_ack','Quiz Entry Acknowledgment','We got your quiz submission!',
 'Thanks {{first_name}}! Your quiz submission on {{campaign_name}} is confirmed. We are calculating your results now.',
 '<h2>Submission received!</h2><p>Thanks {{first_name}} — your entry in <b>{{campaign_name}}</b> is confirmed. We are calculating your results now.</p>', true, NULL),
('score_reveal','Quiz Score & Outcome','Your quiz results are in!',
 'Your score on {{campaign_name}}: {{user_score}}. {{reward_code}}',
 '<h2>Your results are in!</h2><p>Score: <b>{{user_score}}</b></p>{{#if reward_code}}<p>Reward code: <b>{{reward_code}}</b></p>{{/if}}', true, NULL),
('challenge_share','Quiz Challenge & Share','Think you can beat {{user_score}}?',
 'Challenge your friends to beat your {{campaign_name}} score of {{user_score}}. Share now!',
 '<h2>Beat this score!</h2><p>You scored <b>{{user_score}}</b> on {{campaign_name}}. Challenge your friends: <b>{{share_link}}</b></p>', true, NULL),

-- ================= POLL =================
('vote_confirm','Poll Vote Confirmation','Thanks for voting!',
 'Thanks {{first_name}} — your vote on {{campaign_name}} is locked in.',
 '<h2>Vote locked in!</h2><p>Thanks for voting on <b>{{campaign_name}}</b>, {{first_name}}.</p>', true, NULL),
('results_update','Poll Results Update','The {{campaign_name}} results are in',
 'Results for {{campaign_name}} are now available. Here is how the community voted.',
 '<h2>Results are in</h2><p>See how the community voted on <b>{{campaign_name}}</b> compared to your pick.</p>', true, NULL),
('next_topic','Next Poll Invitation','We want your take on our next poll',
 'Share your opinion on our next community poll related to {{campaign_name}}.',
 '<h2>Weigh in on the next topic</h2><p>Your voice matters — join our next poll.</p>', true, NULL),

-- ================= SPIN WHEEL =================
('win_voucher','Spin Win Voucher','You won! Here is your prize',
 'You won {{prize_name}} on {{campaign_name}}! Code: {{reward_code}}. Expires {{expiry_date}}.',
 '<h2>You won!</h2><p>Prize: <b>{{prize_name}}</b></p><p>Code: <b>{{reward_code}}</b></p><p>Expires: <b>{{expiry_date}}</b></p>', true, NULL),
('redemption_reminder','Spin Reward Expiring Soon','Your reward expires soon!',
 'Your {{prize_name}} code {{reward_code}} expires {{expiry_date}}. Redeem before it is gone!',
 '<h2>Hurry!</h2><p>Your <b>{{prize_name}}</b> (code {{reward_code}}) expires {{expiry_date}}.</p>', true, NULL),
('post_redemption_thanks','Thanks for Redeeming','Thanks for redeeming your reward!',
 'Thanks {{first_name}} for redeeming {{prize_name}}! Want another chance? Spin again!',
 '<h2>Thanks for redeeming!</h2><p>Enjoy your {{prize_name}}. Want another chance? <b>{{share_link}}</b></p>', true, NULL),

-- ================= RAFFLE =================
('entry_ticket','Raffle Entry & Ticket','Your raffle tickets are in!',
 'You are entered in {{campaign_name}}. Ticket #: {{ticket_number}}.',
 '<h2>You are entered!</h2><p>Ticket #<b>{{ticket_number}}</b> for {{campaign_name}}.</p>', true, NULL),
('bonus_entry_prompt','Earn Bonus Raffle Entries','Earn extra entries!',
 'Share your referral link {{referral_link}} to earn bonus entries in {{campaign_name}}.',
 '<h2>Earn bonus entries!</h2><p>Share your link: <b>{{referral_link}}</b></p>', true, NULL),
('winner_announcement','Raffle Winner Announcement','And the winner is…',
 'Congratulations! You won {{campaign_name}}! Prize: {{prize_name}}.',
 '<h2>Congratulations!</h2><p>You won <b>{{campaign_name}}</b>! Prize: <b>{{prize_name}}</b></p>', true, NULL),
('closing_notice','Raffle Closing Notice','The {{campaign_name}} raffle has concluded',
 'Thanks for participating in {{campaign_name}}. Here is a consolation offer for you.',
 '<h2>The raffle has concluded</h2><p>Thanks for playing {{campaign_name}} — enjoy this consolation offer.</p>', true, NULL),

-- ================= SURVEY =================
('submission_thanks','Survey Submission Thanks','Thanks for your feedback!',
 'Thanks {{first_name}} for completing {{campaign_name}}. Your entry is confirmed.',
 '<h2>Thank you!</h2><p>Your feedback on {{campaign_name}} is in.</p>', true, NULL),
('reward_delivery','Survey Reward Delivery','Here is your reward',
 'Thanks for completing {{campaign_name}}. Your reward: {{reward_code}}.',
 '<h2>Your reward</h2><p>Code: <b>{{reward_code}}</b></p>', true, NULL),
('impact_report','Survey Impact Report','How your feedback is being used',
 'Here is how your feedback on {{campaign_name}} is making an impact.',
 '<h2>Your feedback at work</h2><p>See how {{campaign_name}} is using your input.</p>', true, NULL),

-- ================= CALCULATOR =================
('calc_summary','Calculation Summary','Your {{campaign_name}} results',
 'Here is your {{campaign_name}} calculation result: {{user_score}}.',
 '<h2>Your calculation</h2><p>Result: <b>{{user_score}}</b></p>', true, NULL),
('consultation_offer','Deep-Dive Consultation','Explore your results further',
 'Want a deeper look at your {{campaign_name}} results? Book a consultation.',
 '<h2>Go deeper</h2><p>Interested in a follow-up on your result? We are here to help.</p>', true, NULL),
('re_run_prompt','Re-run Your Calculation','Parameters changed? Recalculate now',
 'Come back to recalculate your {{campaign_name}} results as conditions change.',
 '<h2>Re-run now</h2><p>Updated parameters? Recalculate your {{campaign_name}} results.</p>', true, NULL),

-- ================= B2B LOYALTY (generic — any business) =================
('welcome_listing','Loyalty Program Welcome','Welcome to our loyalty program!',
 'Welcome {{first_name}} to our loyalty program ({{campaign_name}})! Here is how to start earning.',
 '<h2>Welcome to the loyalty program!</h2><p>{{first_name}}, start earning perks with {{campaign_name}}.</p>', true, NULL),
('loyalty_digest','Loyalty Perks Digest','Your monthly loyalty update',
 'Here is your {{campaign_name}} loyalty activity and perks update.',
 '<h2>Your monthly update</h2><p>See your engagement and perks for {{campaign_name}}.</p>', true, NULL),
('community_spotlight','Community Spotlight','You are in the spotlight!',
 'Congratulations — you have been featured in the {{campaign_name}} community spotlight!',
 '<h2>Spotlight!</h2><p>You are featured in the {{campaign_name}} community.</p>', true, NULL),

-- ================= IQS =================
('submission_receipt','IQS Submission Receipt','We received your application',
 'Thanks {{first_name}} — your qualification submission for {{campaign_name}} was received.',
 '<h2>Application received</h2><p>Thanks {{first_name}}, we got your submission.</p>', true, NULL),
('qualification_approved','Qualification Approved','You are qualified!',
 'Great news {{first_name}} — you qualified for {{campaign_name}}! Next steps inside.',
 '<h2>You are qualified!</h2><p>Great news — here are your next steps.</p>', true, NULL),
('qualification_next_steps','Qualification Next Steps','Your qualification next steps',
 'Here are your next steps for {{campaign_name}} and resources to help you qualify.',
 '<h2>Next steps</h2><p>Here is how to move forward with {{campaign_name}}.</p>', true, NULL),
('nurture_followup','Nurture Follow-Up','Stay in the loop',
 'Did not fully qualify for {{campaign_name}}? Here are resources to help you get there.',
 '<h2>Warm up your lead</h2><p>Here are resources to help you qualify for {{campaign_name}}.</p>', true, NULL),

-- ================= MYSTERY =================
('mystery_secured','Mystery Box Secured','Your mystery box is locked in!',
 'Your mystery box for {{campaign_name}} is secured. What could be inside?',
 '<h2>Box secured!</h2><p>Your mystery box is locked in. Stay tuned…</p>', true, NULL),
('big_reveal','The Big Reveal','Your mystery prize is revealed!',
 'You unlocked {{prize_name}}! Redeem once: {{redeem_link}}.',
 '<h2>The reveal!</h2><p>You unlocked <b>{{prize_name}}</b>!</p><p>Redeem: <b>{{redeem_link}}</b></p>', true, NULL),
('urgent_expiry_notice','Mystery Perk Expiring','Your mystery perk is about to disappear!',
 'Your {{prize_name}} is about to expire. Redeem now!',
 '<h2>Expiring soon!</h2><p>Redeem your {{prize_name}} before it disappears.</p>', true, NULL),

-- ================= COUNTDOWN =================
('registration_lockin','Countdown Registration','You are locked in!',
 'You are registered and tracking {{campaign_name}}. Get ready!',
 '<h2>You are in!</h2><p>You are now tracking {{campaign_name}}.</p>', true, NULL),
('final_countdown','Final Countdown','Time is running out!',
 '{{campaign_name}} is about to close. Act now before the timer runs out!',
 '<h2>Final countdown!</h2><p>Act now — {{campaign_name}} is closing soon.</p>', true, NULL),
('post_deadline_followup','Second Chance','You missed it — but here is another chance',
 'Missed {{campaign_name}}? Here is a second-chance option.',
 '<h2>Second chance</h2><p>Early access to the next timed event is yours.</p>', true, NULL),

-- ================= SCORE REVEAL =================
('processing_notice','Score Processing Notice','Your score is being prepared',
 'Your {{campaign_name}} score is being processed. Stay tuned for the reveal!',
 '<h2>Processing…</h2><p>Your {{campaign_name}} score is on its way.</p>', true, NULL),
('official_score_release','Official Score Release','Your official score is here',
 'Your {{campaign_name}} score: {{user_score}}. Tier: {{tier_name}}.',
 '<h2>Your score</h2><p>Score: <b>{{user_score}}</b></p><p>Tier: <b>{{tier_name}}</b></p>', true, NULL),
('improvement_roadmap','Improvement Roadmap','Your path to a better score',
 'Here are actionable next steps to improve your {{campaign_name}} score.',
 '<h2>Improve your score</h2><p>Here is your roadmap for {{campaign_name}}.</p>', true, NULL),

-- ================= SCRATCH =================
('scratch_confirm_prize','Scratch Prize Delivery','You scratched a winner!',
 'You won {{prize_name}}! Code: {{reward_code}}. Expires {{expiry_date}}.',
 '<h2>You scratched a winner!</h2><p>Prize: <b>{{prize_name}}</b></p><p>Code: <b>{{reward_code}}</b></p>', true, NULL),
('expiry_warning','Scratch Reward Expiring','Your scratch reward expires soon',
 'Your {{prize_name}} code {{reward_code}} expires {{expiry_date}}.',
 '<h2>Almost gone!</h2><p>Redeem {{reward_code}} before {{expiry_date}}.</p>', true, NULL),
('second_chance_replay','Scratch Second Chance','Try your luck again!',
 'Ready for another scratch? Join the new {{campaign_name}} cycle!',
 '<h2>Another chance!</h2><p>Play the new {{campaign_name}} cycle.</p>', true, NULL),

-- ================= SECRET CODES =================
('code_accepted_reward','Secret Code Accepted','Your code unlocked a reward!',
 'Your secret code is valid! Reward: {{reward_code}}.',
 '<h2>Code accepted!</h2><p>Reward unlocked: <b>{{reward_code}}</b></p>', true, NULL),
('code_expiring_soon','Secret Code Expiring','Use your code before it expires',
 'Your unlocked perk {{reward_code}} expires {{expiry_date}}.',
 '<h2>Use it soon</h2><p>{{reward_code}} expires {{expiry_date}}.</p>', true, NULL),
('next_code_hint','Next Code Hint','A hint for the next code…',
 'Here is a hint for the next hidden {{campaign_name}} code…',
 '<h2>Hint drop</h2><p>A hint for the next hidden code is here.</p>', true, NULL),

-- ================= TIER =================
('tier_status_assign','Tier Status','Welcome to {{tier_name}}!',
 'You are now {{tier_name}} tier in {{campaign_name}}. Here are your unlocked perks.',
 '<h2>Welcome to {{tier_name}}!</h2><p>Here are your unlocked perks.</p>', true, NULL),
('tier_upgrade_progress','Tier Upgrade Progress','You are close to {{next_tier}}!',
 'You are close to reaching {{next_tier}} tier in {{campaign_name}}. Keep going!',
 '<h2>Almost there!</h2><p>You are close to {{next_tier}}. Here is what you need.</p>', true, NULL),
('milestone_reached','Milestone Reached','You leveled up to {{tier_name}}!',
 'Congratulations {{first_name}}! You reached {{tier_name}} tier in {{campaign_name}}. Bonus reward inside.',
 '<h2>Level up!</h2><p>You reached <b>{{tier_name}}</b>! Bonus reward inside.</p>', true, NULL),

-- ================= LONG-FORM QUALIFIER =================
('application_received','Application Received','We received your application',
 'Your detailed submission for {{campaign_name}} has been safely logged.',
 '<h2>Application received</h2><p>Your submission is safely logged.</p>', true, NULL),
('review_complete_decision','Review Complete','Your qualification decision is ready',
 'Your {{campaign_name}} qualification decision is ready. Here are your next steps.',
 '<h2>Decision ready</h2><p>Here is the outcome for {{campaign_name}}.</p>', true, NULL),
('further_info_request','Further Information Request','We need a bit more info',
 'We need additional information to complete your {{campaign_name}} application.',
 '<h2>More info needed</h2><p>Please provide additional details for {{campaign_name}}.</p>', true, NULL)
ON CONFLICT DO NOTHING;
