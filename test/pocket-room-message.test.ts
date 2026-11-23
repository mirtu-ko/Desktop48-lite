import { describe, expect, it } from 'vitest'
import {
  isPlaceholderText,
  liveShareTitle,
  normalizePocketRoomMessage,
  pocketRoomCopyTargets,
  type PocketRoomMessageView,
} from '@renderer/utils/pocket-room-message'

/**
 * 房间消息归一化回归。
 *
 * 房间页不提供发消息，只负责把服务端报文渲染对：字段来源、媒体直链、
 * 占位文案与「可复制内容」这几条规则一旦漂移，用户看到的要么是错位的气泡，
 * 要么是复制出 `[图片消息]` 这种垃圾。这里按真实报文的形状固定住。
 */

/** 只填 pocketRoomCopyTargets 关心的字段 */
function view(overrides: Partial<PocketRoomMessageView>): PocketRoomMessageView {
  return {
    id: '1',
    msgIdClient: '',
    msgIdServer: '1',
    kind: 'text',
    text: '',
    userId: '1',
    nick: '口袋用户',
    avatar: '',
    level: 0,
    isMember: false,
    msgTime: 0,
    quote: null,
    imageUrl: '',
    imageWidth: 0,
    imageHeight: 0,
    audioUrl: '',
    videoUrl: '',
    videoPoster: '',
    videoWidth: 0,
    videoHeight: 0,
    liveId: '',
    liveType: 0,
    liveMode: 0,
    liveStartTime: 0,
    liveCover: '',
    liveIsPlayback: false,
    ...overrides,
  }
}

/** 服务端 TEXT 消息的真实形状：正文是 JSON 字符串，发送者资料在 extInfo.user */
function textMessage(text: string, extra: Record<string, unknown> = {}) {
  return {
    msgIdClient: 'a1b2c3',
    msgIdServer: '7505138435',
    msgTime: 1795351274409,
    msgType: 'TEXT',
    bodys: JSON.stringify({ text }),
    extInfo: JSON.stringify({ user: { userId: 103679171, nickName: '小粒lll', avatar: '/avatar/1.png', level: 6, roleId: 0 } }),
    ...extra,
  }
}

describe('normalizePocketRoomMessage', () => {
  it('文本消息：正文、发送者、ID 都从真实位置取', () => {
    const message = normalizePocketRoomMessage(textMessage('晚上好'))!
    expect(message.kind).toBe('text')
    expect(message.text).toBe('晚上好')
    expect(message.nick).toBe('小粒lll')
    expect(message.userId).toBe('103679171')
    expect(message.level).toBe(6)
    expect(message.isMember).toBe(false)
    expect(message.id).toBe('a1b2c3')
    expect(message.msgTime).toBe(1795351274409)
  })

  it('roleId > 1 标记为成员', () => {
    const raw = textMessage('hi', {
      extInfo: JSON.stringify({ user: { userId: 1, nickName: '成员', roleId: 2 } }),
    })
    expect(normalizePocketRoomMessage(raw)!.isMember).toBe(true)
  })

  it('缺少发送者资料时回落到占位昵称，不抛错', () => {
    const message = normalizePocketRoomMessage({ msgType: 'TEXT', bodys: '{"text":"hi"}' })!
    expect(message.nick).toBe('口袋用户')
    expect(message.userId).toBe('')
  })

  it('DELETE 通知不展示', () => {
    expect(normalizePocketRoomMessage({ msgType: 'DELETE' })).toBeNull()
  })

  it('图片消息：相对路径挂 source CDN，尺寸用于占位', () => {
    const raw = {
      msgType: 'IMAGE',
      bodys: JSON.stringify({ url: '/image/2026/01/a.jpg', width: 800, height: 600 }),
      extInfo: JSON.stringify({ user: { userId: 1, nickName: 'x' } }),
    }
    const message = normalizePocketRoomMessage(raw)!
    expect(message.kind).toBe('image')
    expect(message.imageUrl).toBe('https://source.48.cn/image/2026/01/a.jpg')
    expect(message.imageWidth).toBe(800)
    expect(message.imageHeight).toBe(600)
  })

  it('已是绝对地址的媒体不改写', () => {
    const raw = {
      msgType: 'IMAGE',
      bodys: JSON.stringify({ url: 'https://cdn.example.com/a.jpg' }),
      extInfo: '{}',
    }
    expect(normalizePocketRoomMessage(raw)!.imageUrl).toBe('https://cdn.example.com/a.jpg')
  })

  it('语音消息走 mp4 CDN', () => {
    const raw = {
      msgType: 'AUDIO',
      bodys: JSON.stringify({ url: '/audio/a.mp4' }),
      extInfo: '{}',
    }
    expect(normalizePocketRoomMessage(raw)!.audioUrl).toBe('https://mp4.48.cn/audio/a.mp4')
  })

  it('回复消息带引用块（被引用内容单独成块，不进正文）', () => {
    const raw = {
      msgType: 'REPLY',
      bodys: JSON.stringify({
        text: '是的',
        replyInfo: JSON.stringify({ replyName: '泡沫の', replyText: '明天播吗' }),
      }),
      extInfo: '{}',
    }
    const message = normalizePocketRoomMessage(raw)!
    expect(message.kind).toBe('reply')
    expect(message.text).toBe('是的')
    expect(message.quote?.nick).toBe('泡沫の')
    expect(message.quote?.text).toBe('明天播吗')
  })

  it('bodys 是裸文本（非 JSON）时整体作正文', () => {
    const message = normalizePocketRoomMessage({ msgType: 'TEXT', bodys: ' 裸文本 ' })!
    expect(message.text).toBe('裸文本')
  })

  it('未知类型不丢：默认按文本展示（协议新增类型时不整条消失）', () => {
    const message = normalizePocketRoomMessage({ msgType: 'SOMETHING_NEW', extInfo: '{}' })!
    expect(message).not.toBeNull()
    expect(message.kind).toBe('text')
    expect(message.text).toBe('[新消息]')
  })

  it('禁言 / 关房通知归为 system', () => {
    expect(normalizePocketRoomMessage({ msgType: 'DISABLE_SPEAK', extInfo: '{}' })!.kind)
      .toBe('system')
    expect(normalizePocketRoomMessage({ msgType: 'CLOSE_ROOM_CHAT', extInfo: '{}' })!.kind)
      .toBe('system')
  })

  it('SHARE_LIVE 的 liveId 从 jumpPath 里提取', () => {
    const raw = {
      msgType: 'SHARE_LIVE',
      bodys: JSON.stringify({
        shareInfo: { jumpPath: '/live/detail?id=1330968594399170560', liveCover: '/cover/1.jpg' },
      }),
      extInfo: '{}',
    }
    const message = normalizePocketRoomMessage(raw)!
    expect(message.kind).toBe('live')
    expect(message.liveId).toBe('1330968594399170560')
    expect(message.liveCover).toBe('https://source.48.cn/cover/1.jpg')
    expect(message.liveIsPlayback).toBe(false)
  })

  it('PLAYBACK 类分享标记为回放', () => {
    const raw = {
      msgType: 'SHARE_PLAYBACK',
      bodys: JSON.stringify({ shareInfo: { liveId: '42' } }),
      extInfo: '{}',
    }
    expect(normalizePocketRoomMessage(raw)!.liveIsPlayback).toBe(true)
  })

  it('非对象输入不 panic', () => {
    for (const raw of [null, undefined, 0, 'x', []]) {
      expect(() => normalizePocketRoomMessage(raw)).not.toThrow()
    }
  })
})

describe('占位文案与复制目标', () => {
  it('占位文案与空串都算「没有正文」', () => {
    expect(isPlaceholderText('[图片消息]')).toBe(true)
    expect(isPlaceholderText('')).toBe(true)
    expect(isPlaceholderText('真的说的话')).toBe(false)
  })

  it('文本消息复制正文', () => {
    expect(pocketRoomCopyTargets(view({ text: '晚上好' }))).toEqual([
      { key: 'text', text: '晚上好' },
    ])
  })

  it('图片消息只复制直链，不复制占位正文', () => {
    expect(
      pocketRoomCopyTargets(view({ kind: 'image', text: '[图片消息]', imageUrl: 'https://s/a.jpg' })),
    ).toEqual([{ key: 'image', text: 'https://s/a.jpg' }])
  })

  it('礼物 / 直播分享的合成正文不参与复制', () => {
    expect(pocketRoomCopyTargets(view({ kind: 'gift', text: '送了 1 个爱心' }))).toEqual([])
    expect(pocketRoomCopyTargets(view({ kind: 'live', text: '[直播分享] 开播了' }))).toEqual([])
  })

  it('正文在前、媒体直链在后', () => {
    expect(
      pocketRoomCopyTargets(view({ kind: 'reply', text: '是的', imageUrl: 'https://s/a.jpg', audioUrl: 'https://v/a.mp4' })),
    ).toEqual([
      { key: 'text', text: '是的' },
      { key: 'image', text: 'https://s/a.jpg' },
      { key: 'audio', text: 'https://v/a.mp4' },
    ])
  })
})

describe('liveShareTitle', () => {
  it('剥掉归一层加的前缀并去空白', () => {
    expect(liveShareTitle('[直播分享]   今晚八点开播 ')).toBe('今晚八点开播')
    expect(liveShareTitle('没有前缀')).toBe('没有前缀')
  })
})
